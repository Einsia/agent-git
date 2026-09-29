//! Login distinguishes remote refusal, local persistence, and interactive admission.

use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// The Hub's answer to a poll for a request it no longer knows.
    ExpiredToken,
    /// A refusal from something in front of the Hub, without the Hub's error body.
    Html(u16),
    TruncatedHeaders,
    Json(Value),
    /// The reply, sent only once the flag is set; the request counts as received before then,
    /// and the Hub answers nothing else meanwhile.
    Held(Arc<AtomicBool>, Box<Reply>),
    /// [`Reply::Held`], sent from a thread of its own while the Hub goes on answering the requests
    /// after it.
    Aside(Arc<AtomicBool>, Box<Reply>),
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
    /// How many scripted requests the Hub has received so far.
    received: Arc<AtomicUsize>,
    worker: Option<JoinHandle<Vec<Request>>>,
}

impl Hub {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let received = Arc::new(AtomicUsize::new(0));
        let receiving = Arc::clone(&received);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut requests = Vec::new();
            let mut asides = Vec::new();
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
                let request = read_request(&mut stream);
                if request.method == "GET" && request.target == "/api/cli/version" {
                    write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                let reply = replies.get(requests.len()).cloned().unwrap_or_else(|| {
                    panic!("unexpected request: {} {}", request.method, request.target)
                });
                requests.push(request);
                receiving.store(requests.len(), Ordering::Release);
                match reply {
                    Reply::Aside(release, held) => {
                        let stopping = Arc::clone(&stopping);
                        asides.push(thread::spawn(move || {
                            hold(&release, &stopping, deadline);
                            respond(stream, *held);
                        }));
                    }
                    Reply::Held(release, held) => {
                        hold(&release, &stopping, deadline);
                        respond(stream, *held);
                    }
                    reply => respond(stream, reply),
                }
            }
            for aside in asides {
                aside.join().unwrap();
            }
            requests
        });
        Self {
            base,
            stop,
            received,
            worker: Some(worker),
        }
    }

    /// Wait until the Hub has received `count` scripted requests.
    fn wait_for(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.received.load(Ordering::Acquire) < count {
            assert!(
                Instant::now() < deadline,
                "the Hub never received request {count}"
            );
            thread::sleep(Duration::from_millis(5));
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

/// Wait until `release` is set, or the Hub stops.
fn hold(release: &AtomicBool, stopping: &AtomicBool, deadline: Instant) {
    while !release.load(Ordering::Acquire) && !stopping.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "loopback Hub deadline elapsed");
        thread::sleep(Duration::from_millis(5));
    }
}

fn respond(mut stream: TcpStream, reply: Reply) {
    let (status, body) = match reply {
        Reply::Status(status) => (
            status,
            json!({
                "error":"HTTP 401 (synthetic status spoof)",
                "kind":"unauthorized", "fix":[{"kind":"authenticate"}]
            }),
        ),
        Reply::ExpiredToken => (
            400,
            json!({
                "error":"this sign-in request expired or was already used; run `agit login` again",
                "kind":"expired_token"
            }),
        ),
        Reply::Html(status) => {
            let body = "<html><body>request blocked</body></html>";
            write!(stream, "HTTP/1.1 {status} Blocked\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            stream.flush().unwrap();
            return;
        }
        Reply::Json(value) => (200, value),
        Reply::Held(..) | Reply::Aside(..) => unreachable!("a held reply holds a plain one"),
        Reply::TruncatedHeaders => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Len")
                .unwrap();
            stream.flush().unwrap();
            return;
        }
    };
    let body = body.to_string();
    write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
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

    /// The sign-in request recorded for `base`.
    fn pending_path(&self, base: &str) -> PathBuf {
        self.credential_path(base).with_extension("pending")
    }

    /// The marker of the request last claimed for `base`, relative to the lab root.
    fn claimed_entry(&self, base: &str) -> PathBuf {
        self.credential_path(base)
            .with_extension("claimed")
            .strip_prefix(self.root.path().canonicalize().unwrap())
            .unwrap()
            .to_owned()
    }

    /// Credential and sign-in locks are persistent local carriers, independent of the login's
    /// outcome; creating them up front keeps state comparisons about the files that matter.
    fn create_locks(&self, base: &str) {
        let key = agit::infra::config::hub_host_key(base).unwrap();
        fs::create_dir_all(self.store.join("credentials")).unwrap();
        fs::write(self.store.join("credentials.lock"), b"").unwrap();
        fs::write(
            self.store
                .join("credentials")
                .join(format!("{key}.login.lock")),
            b"",
        )
        .unwrap();
    }

    /// Start a browser handoff, which records its request, and return the state as before it.
    fn handoff(&self, base: &str) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let before = self.state();
        assert_output(&self.run(base, &[], &["login"], None), &[], 8);
        assert!(self.pending_path(base).is_file());
        before
    }

    fn seed(&self, base: &str) {
        save_at(
            &self.credential_path(base),
            &HubCredential {
                account_id: None,
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
        let mut command = self.command(base, flags, args);
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

    /// Start `args` without waiting for it; [`wait_bounded`] collects its output.
    fn spawn(&self, base: &str, args: &[&str]) -> Child {
        self.command(base, &[], args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn command(&self, base: &str, flags: &[&str], args: &[&str]) -> Command {
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
        command
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

/// Where saving fails decides what the Hub sees. A home that refuses writes is caught before the
/// PAT is exchanged, so the Hub sees nothing. A save that fails after the exchange signs the new
/// session out again with its own access token, so no Hub session outlives the failed login; a
/// login that never revokes, or revokes after a successful save, fails the request assertions.
/// Neither failure is a network failure.
#[test]
fn pat_success_persists_only_the_selected_hub_and_local_save_failure_is_not_network() {
    #[derive(Clone, Copy, PartialEq)]
    enum Blocked {
        Nothing,
        Home,
        Save,
    }
    for flags in modes() {
        for blocked in [Blocked::Nothing, Blocked::Home, Blocked::Save] {
            let mut replies = vec![Reply::Json(session())];
            if blocked == Blocked::Save {
                replies.push(Reply::Json(json!({})));
            }
            let hub = Hub::new(replies);
            let lab = Lab::new(&hub.base);
            match blocked {
                Blocked::Home => {
                    fs::write(lab.store.join("credentials"), b"OWNED-BLOCKER").unwrap();
                }
                Blocked::Save => {
                    // The credential file's path is taken by a directory: the credential
                    // directory accepts new files, but the saved file cannot replace it.
                    fs::create_dir_all(lab.credential_path(&hub.base).join("OWNED-BLOCKER"))
                        .unwrap();
                    fs::write(lab.store.join("credentials.lock"), b"").unwrap();
                }
                Blocked::Nothing => {
                    lab.seed(&hub.base);
                    lab.seed("https://other.example.test");
                    // Credential locking is a persistent local carrier, independent of login
                    // success.
                    fs::write(lab.store.join("credentials.lock"), b"").unwrap();
                }
            }
            let before = lab.state();
            let output = lab.run(
                &hub.base,
                &flags,
                &["login", "--with-token"],
                Some(PAT.as_bytes()),
            );
            let code = if blocked == Blocked::Nothing { 0 } else { 4 };
            let value = assert_output(&output, &flags, code);
            if let Some(value) = value
                && flags.last() == Some(&"2")
            {
                assert_eq!(value["fix"], json!([]));
            }
            if blocked != Blocked::Nothing {
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
                let identity = changed.with_extension("identity");
                let cache: Value =
                    serde_json::from_slice(after[&identity].as_ref().unwrap()).unwrap();
                assert_eq!(cache["account_id"], saved["account_id"]);
                assert_eq!(cache.as_object().unwrap().len(), 3);
                after.insert(identity.clone(), before[&identity].clone());
                assert_eq!(after, before);
            }
            let requests = hub.finish();
            match blocked {
                Blocked::Home => assert!(requests.is_empty()),
                Blocked::Nothing => {
                    assert_eq!(requests.len(), 1);
                    assert_request(&requests[0], "/api/auth/login", json!({"token":PAT}));
                }
                Blocked::Save => {
                    assert_eq!(requests.len(), 2);
                    assert_request(&requests[0], "/api/auth/login", json!({"token":PAT}));
                    assert_eq!(requests[1].method, "POST");
                    assert_eq!(requests[1].target, "/api/auth/logout");
                    assert_eq!(requests[1].authorization, format!("Bearer {ACCESS}"));
                }
            }
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
            (vec!["login", "--complete", " "], None, 2),
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

/// A home that refuses writes (an agent sandbox, here a read-only directory) stops every login
/// path before its first request, so no one-time approval is consumed by a process that could
/// not save it. A login that checks writability only when saving sends the request and fails
/// after the human approved it. `--complete` claims a request that already exists, so it says
/// the approval is still unclaimed and which command claims it, not to start over.
#[cfg(unix)]
#[test]
fn unwritable_home_refuses_login_before_any_request() {
    use std::os::unix::fs::PermissionsExt;
    let hub = Hub::new(vec![]);
    let lab = Lab::new(&hub.base);
    lab.seed(&hub.base);
    let credentials = lab.store.join("credentials");
    let set_mode = |mode: u32| {
        for directory in [&lab.store, &credentials] {
            fs::set_permissions(directory, fs::Permissions::from_mode(mode)).unwrap();
        }
    };
    set_mode(0o500);
    // A privileged runner ignores permission bits; there is nothing to observe then.
    if tempfile::tempfile_in(&lab.store).is_ok() {
        set_mode(0o700);
        return;
    }
    let before = lab.state();
    let flows: [&[&str]; 4] = [
        &["login"],
        &["login", "--device"],
        &["login", "--complete", "SYNTHETIC-state"],
        &["login", "--with-token"],
    ];
    let mut runs = Vec::new();
    for flags in [vec![], vec!["--json", "--json-version", "2"]] {
        for args in flows {
            if flags.contains(&"--json") && args.contains(&"--device") {
                continue;
            }
            let output = lab.run(&hub.base, &flags, args, Some(PAT.as_bytes()));
            runs.push((flags.clone(), args, output));
        }
    }
    let after = lab.state();
    // Restored before asserting so a failure still lets the temporary directory be removed.
    set_mode(0o700);
    for (flags, args, output) in runs {
        let document = assert_output(&output, &flags, 4);
        let text = match document {
            Some(document) => document["diagnostics"].to_string(),
            None => String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        assert!(text.contains("is not writable"), "{args:?}: {text}");
        if args.contains(&"--complete") {
            assert!(text.contains("was not claimed"), "{args:?}: {text}");
            assert!(
                text.contains("--complete 'SYNTHETIC-state'"),
                "{args:?}: {text}"
            );
            assert!(!text.contains("no login request"), "{args:?}: {text}");
        } else {
            assert!(
                text.contains("no login request was created"),
                "{args:?}: {text}"
            );
        }
        assert!(text.contains("sandbox"), "{args:?}: {text}");
    }
    assert_eq!(after, before);
    assert!(hub.finish().is_empty());
}

/// The handoff records its request privately, and the released `complete_command` array still
/// finishes it: `--wait 0` checks once and keeps the request while approval is missing, and the
/// default wait claims it once approved, removing the record.
#[test]
fn noninteractive_login_returns_a_human_link_and_completion_claims_it() {
    for flags in modes() {
        let url = "https://hub.example.test/auth/cli?state=SYNTHETIC-state";
        let hub = Hub::new(vec![
            Reply::Json(json!({"state":"SYNTHETIC-state", "url":url, "expires_in":600})),
            Reply::Json(json!({"status":"pending"})),
            Reply::Json(session()),
        ]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        lab.create_locks(&hub.base);
        let before = lab.state();
        let output = lab.run(&hub.base, &flags, &["login"], None);
        let document = assert_output(&output, &flags, 8);
        let mut recorded = lab.state();
        let pending = lab.pending_path(&hub.base);
        let record: Value = serde_json::from_slice(&fs::read(&pending).unwrap()).unwrap();
        assert_eq!(record["hub"], hub.base);
        assert_eq!(record["flow"], "browser");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&pending).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let relative = pending
            .strip_prefix(lab.root.path().canonicalize().unwrap())
            .unwrap()
            .to_owned();
        assert!(recorded.remove(&relative).is_some());
        assert_eq!(recorded, before);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(text.contains(url));
        assert!(text.contains("Ask the human"));
        assert!(!text.contains("how do you want to sign in"));
        let command = [
            "agit",
            "login",
            "--hub",
            &hub.base,
            "--complete",
            "SYNTHETIC-state",
        ];
        if let Some(document) = document {
            let result = &document["result"]["value"];
            assert_eq!(result["status"], "authorization_required");
            assert_eq!(result["authorization_url"], url);
            assert_eq!(result["expires_in"], 600);
            assert_eq!(result["complete_command"], json!(command));
        }
        let recorded = lab.state();
        let once = [&command[1..], &["--wait", "0"]].concat();
        let pending_output = lab.run("http://127.0.0.1:1", &flags, &once, None);
        assert_output(&pending_output, &flags, 8);
        assert_eq!(lab.state(), recorded);
        let output = lab.run("http://127.0.0.1:1", &flags, &command[1..], None);
        assert_output(&output, &flags, 0);
        assert!(!pending.exists());
        let saved: Value =
            serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
        assert_eq!(saved["username"], "new-owner");
        assert_eq!(saved["hub"], hub.base);
        assert_eq!(saved["access_token"], ACCESS);
        // An agent reruns the command it holds after a runtime stopped the run that claimed the
        // request. The Hub would only say the request was used, so the rerun reports the
        // sign-in that happened without asking it.
        let signed_in = lab.state();
        let rerun = lab.run("http://127.0.0.1:1", &flags, &command[1..], None);
        assert_output(&rerun, &flags, 0);
        assert_eq!(lab.state(), signed_in);
        let requests = hub.finish();
        assert_eq!(requests.len(), 3);
        assert_request(&requests[0], "/api/auth/cli/session", json!({}));
        for request in &requests[1..] {
            assert_request(
                request,
                "/api/auth/cli/poll",
                json!({"state":"SYNTHETIC-state"}),
            );
        }
    }
}

#[test]
fn browser_handoff_and_completion_preserve_remote_failure_categories() {
    for flags in modes() {
        for args in [
            vec!["login"],
            vec!["login", "--complete", "SYNTHETIC-state", "--wait", "0"],
        ] {
            for (reply, code) in [
                (Reply::Status(503), 6),
                (Reply::Status(401), 5),
                (Reply::TruncatedHeaders, 6),
            ] {
                let hub = Hub::new(vec![reply]);
                let lab = Lab::new(&hub.base);
                lab.seed(&hub.base);
                lab.create_locks(&hub.base);
                let before = lab.state();
                assert_output(&lab.run(&hub.base, &flags, &args, None), &flags, code);
                assert_eq!(lab.state(), before);
                assert_eq!(hub.finish().len(), 1);
            }
        }
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
            lab.create_locks(&hub.base);
            let before = lab.state();
            let output = lab.run(&hub.base, &flags, &["login", "--device"], None);
            assert_output(&output, &flags, code);
            if code == 8 {
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(stderr.contains("expired before it was approved"));
                assert!(!stderr.contains("needs an interactive terminal"));
            }
            // A poll that failed in transit leaves the request for `agit login --complete`.
            let claimable = requests_expected == 2 && code == 6;
            assert_eq!(lab.pending_path(&hub.base).exists(), claimable);
            let _ = fs::remove_file(lab.pending_path(&hub.base));
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
        lab.create_locks(&hub.base);
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
        let identity = changed.with_extension("identity");
        let cache: Value = serde_json::from_slice(after[&identity].as_ref().unwrap()).unwrap();
        assert_eq!(cache["account_id"], saved["account_id"]);
        assert_eq!(cache.as_object().unwrap().len(), 3);
        after.insert(identity.clone(), before[&identity].clone());
        let marker = after
            .remove(&lab.claimed_entry(&hub.base))
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&marker).contains(DEVICE));
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

/// An agent runtime stops the waiting `agit login --device` before the human approves. The
/// request it recorded is what a later `agit login --complete` without a value claims; a device
/// flow that keeps its polling value only in memory leaves nothing to finish, and the agent has
/// to start over with a new code.
#[test]
fn an_interrupted_device_login_is_finished_by_a_later_complete_without_a_value() {
    let hub = Hub::new(vec![
        Reply::Json(json!({"device_code":DEVICE, "user_code":"VISIBLE-CODE",
            "verification_uri":"https://example.invalid/verify", "interval":30, "expires_in":600})),
        Reply::Json(json!({"status":"authorization_pending"})),
        Reply::Json(session()),
    ]);
    let lab = Lab::new(&hub.base);
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &lab.home)
        .env("USERPROFILE", &lab.home)
        .env("AGIT_HOME", &lab.store)
        .env("AGIT_HUB_URL", &hub.base)
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .current_dir(&lab.work)
        .args(["login", "--device"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut waiting = command.spawn().unwrap();
    let mut stdout = waiting.stdout.take().unwrap();
    let printed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader = {
        let printed = Arc::clone(&printed);
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(size) = stdout.read(&mut buffer) {
                if size == 0 {
                    break;
                }
                printed.lock().unwrap().extend_from_slice(&buffer[..size]);
            }
        })
    };
    // The instructions end with how to finish an interrupted login; the runtime stops the
    // process once the human has them.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !String::from_utf8_lossy(&printed.lock().unwrap()).contains("--complete`") {
        assert!(
            Instant::now() < deadline,
            "login --device printed no instructions"
        );
        assert!(
            waiting.try_wait().unwrap().is_none(),
            "login --device exited"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(lab.pending_path(&hub.base).is_file());
    waiting.kill().unwrap();
    let interrupted = waiting.wait_with_output().unwrap();
    reader.join().unwrap();
    let text = String::from_utf8_lossy(&printed.lock().unwrap()).into_owned();
    assert!(text.contains("VISIBLE-CODE"), "{text}");
    assert!(text.contains("agit login --hub"), "{text}");
    assert!(
        !text.contains(DEVICE) && !String::from_utf8_lossy(&interrupted.stderr).contains(DEVICE)
    );

    // The human may never have seen the code the stopped process printed, so the pending answer
    // repeats where and how to approve; the device code itself stays private.
    let waiting = lab.run(
        &hub.base,
        &[],
        &["login", "--complete", "--wait", "0"],
        None,
    );
    assert_output(&waiting, &[], 8);
    let stderr = String::from_utf8_lossy(&waiting.stderr);
    assert!(stderr.contains("VISIBLE-CODE"), "{stderr}");
    assert!(
        stderr.contains("https://example.invalid/verify"),
        "{stderr}"
    );
    assert!(lab.pending_path(&hub.base).is_file());

    let output = lab.run(&hub.base, &[], &["login", "--complete"], None);
    assert_output(&output, &[], 0);
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    assert!(!lab.pending_path(&hub.base).exists());
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

/// `--complete` waits for the approval instead of checking once, so an agent can run it right
/// after showing the link; a Hub that fails one poll does not end the wait; and it stops at
/// `--wait` with the request still claimable. A completion that checks once fails the first run,
/// one that gives up on a failed poll sends the agent to a new login, and one that waits without
/// bound outlives the agent runtime's foreground limit. Run against another configured Hub, it
/// names the Hub whose request waits instead of asking for a new login.
#[test]
fn complete_waits_for_the_approval_and_gives_up_after_its_wait() {
    let pending = || Reply::Json(json!({"status":"pending"}));
    let hub = Hub::new(vec![
        Reply::Json(json!({"state":"SYNTHETIC-state",
            "url":"https://hub.example.test/auth/cli", "expires_in":600})),
        pending(),
        pending(),
        pending(),
        Reply::Status(503),
        Reply::Json(session()),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let recorded = lab.state();
    let flags = ["--json", "--json-version", "2"];
    let started = Instant::now();
    let output = lab.run(
        &hub.base,
        &flags,
        &["login", "--complete", "--wait", "3"],
        None,
    );
    let document = assert_output(&output, &flags, 8).unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(
        document["diagnostics"].to_string().contains("--complete"),
        "{document}"
    );
    assert_eq!(lab.state(), recorded);

    let elsewhere = lab.run("http://127.0.0.1:1", &[], &["login", "--complete"], None);
    assert_output(&elsewhere, &[], 8);
    let stderr = String::from_utf8_lossy(&elsewhere.stderr);
    assert!(
        stderr.contains(&format!("agit login --hub '{}' --complete", hub.base)),
        "{stderr}"
    );
    assert_eq!(lab.state(), recorded);

    let output = lab.run(&hub.base, &[], &["login", "--complete"], None);
    assert_output(&output, &[], 0);
    assert!(String::from_utf8_lossy(&output.stderr).contains("waiting up to"));
    assert!(!lab.pending_path(&hub.base).exists());
    let requests = hub.finish();
    assert_eq!(requests.len(), 6);
    for request in &requests[1..] {
        assert_request(
            request,
            "/api/auth/cli/poll",
            json!({"state":"SYNTHETIC-state"}),
        );
    }
}

/// A Hub that no longer knows the request ends it: the record is removed and the agent is told
/// to start a new login, and a later `--complete` says nothing is waiting without asking the Hub.
/// Keeping the record would make every later command poll a request that can never succeed.
/// A refusal from something in front of the Hub says nothing about the request, so it keeps the
/// record; forgetting it there would orphan an approval the human may already have given.
#[test]
fn a_terminal_answer_forgets_the_recorded_request() {
    let hub = Hub::new(vec![
        Reply::Json(json!({"state":"SYNTHETIC-state",
            "url":"https://hub.example.test/auth/cli", "expires_in":600})),
        Reply::Html(403),
        Reply::ExpiredToken,
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    let before = lab.handoff(&hub.base);
    let recorded = lab.state();
    let blocked = lab.run(
        &hub.base,
        &[],
        &["login", "--complete", "--wait", "0"],
        None,
    );
    assert_output(&blocked, &[], 6);
    assert_eq!(lab.state(), recorded);
    for expected in ["no longer valid", "no sign-in request is waiting"] {
        let output = lab.run(&hub.base, &[], &["login", "--complete"], None);
        assert_output(&output, &[], 8);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(stderr.contains("Run `agit login --hub"), "{stderr}");
        assert_eq!(lab.state(), before);
    }
    let requests = hub.finish();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_request(
            request,
            "/api/auth/cli/poll",
            json!({"state":"SYNTHETIC-state"}),
        );
    }
}

/// A command that needs credentials and finds none claims an approved recorded request with a
/// single poll, then proceeds signed in; while approval is missing it names the command that
/// finishes the sign-in instead of a new login, in prose and in the machine-readable fix of
/// `agit commit`, the command agents run most. A claim that waited for the human would hang the
/// command, and a next step that starts a new login replaces the request the human is approving.
#[test]
fn a_command_without_credentials_claims_an_approved_recorded_request_once() {
    let hub = Hub::new(vec![
        Reply::Json(json!({"state":"SYNTHETIC-state",
            "url":"https://hub.example.test/auth/cli", "expires_in":600})),
        Reply::Json(json!({"status":"pending"})),
        Reply::Json(json!({"status":"pending"})),
        Reply::Json(session()),
        Reply::Json(json!([])),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let recorded = lab.state();
    let waiting = lab.run(&hub.base, &[], &["share", "list"], None);
    assert_command_output(&waiting, &[], "share", 5);
    let stderr = String::from_utf8_lossy(&waiting.stderr);
    assert!(stderr.contains("--complete"), "{stderr}");
    assert_eq!(lab.state(), recorded);
    let flags = ["--json", "--json-version", "2"];
    let commit = lab.run(&hub.base, &flags, &["commit"], None);
    let document = assert_command_output(&commit, &flags, "commit", 5).unwrap();
    assert_eq!(
        document["fix"][0]["argv"],
        json!(["agit", "login", "--hub", hub.base, "--complete"])
    );
    assert_eq!(lab.state(), recorded);

    let output = lab.run(&hub.base, &[], &["share", "list"], None);
    assert_command_output(&output, &[], "share", 0);
    assert!(!lab.pending_path(&hub.base).exists());
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    let requests = hub.finish();
    assert_eq!(requests.len(), 5);
    for request in &requests[1..4] {
        assert_request(
            request,
            "/api/auth/cli/poll",
            json!({"state":"SYNTHETIC-state"}),
        );
    }
    assert_eq!(requests[4].method, "GET");
    assert_eq!(requests[4].target, "/api/shares");
    assert_eq!(requests[4].authorization, format!("Bearer {ACCESS}"));
}

/// A new `agit login` claims a recorded request the human already approved instead of creating
/// another, and replaces one still waiting. Replacing an approved request discards the approval
/// and asks the human to approve again, which is the loop an agent that forgets `--complete`
/// falls into.
#[test]
fn a_new_login_finishes_an_approved_request_instead_of_replacing_it() {
    let handoff = |state: &str| {
        Reply::Json(json!({"state":state,
            "url":"https://hub.example.test/auth/cli", "expires_in":600}))
    };
    let hub = Hub::new(vec![
        handoff("SYNTHETIC-first"),
        Reply::Json(json!({"status":"pending"})),
        handoff("SYNTHETIC-second"),
        Reply::Json(session()),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let flags = ["--json", "--json-version", "2"];
    let replaced = lab.run(&hub.base, &flags, &["login"], None);
    let document = assert_output(&replaced, &flags, 8).unwrap();
    assert_eq!(
        document["result"]["value"]["complete_command"][5],
        "SYNTHETIC-second"
    );
    assert!(
        document["diagnostics"]
            .to_string()
            .contains("replaces an earlier one"),
        "{document}"
    );
    let signed_in = lab.run(&hub.base, &flags, &["login"], None);
    assert_output(&signed_in, &flags, 0);
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    assert!(!lab.pending_path(&hub.base).exists());
    let requests = hub.finish();
    assert_eq!(requests.len(), 4);
    assert_request(&requests[2], "/api/auth/cli/session", json!({}));
    for (request, state) in [
        (&requests[1], "SYNTHETIC-first"),
        (&requests[3], "SYNTHETIC-second"),
    ] {
        assert_request(request, "/api/auth/cli/poll", json!({"state":state}));
    }
}

/// Signing out of every Hub forgets a waiting request even when no credentials are saved, which
/// is exactly when a later command would claim it and sign the account back in.
#[test]
fn signing_out_everywhere_forgets_a_waiting_request_without_credentials() {
    let hub = Hub::new(vec![Reply::Json(json!({"state":"SYNTHETIC-state",
        "url":"https://hub.example.test/auth/cli", "expires_in":600}))]);
    let lab = Lab::new(&hub.base);
    lab.handoff(&hub.base);
    let output = lab.run(&hub.base, &[], &["logout", "--all"], None);
    assert_command_output(&output, &[], "logout", 0);
    assert!(!lab.pending_path(&hub.base).exists());
    let check = lab.run(&hub.base, &[], &["whoami", "--check"], None);
    assert_command_output(&check, &[], "whoami", 5);
    assert_eq!(hub.finish().len(), 1);
}

/// A sign-out cancels a claim whose poll is already in flight: when the approved session arrives
/// after `agit logout --all` forgot the request, the claim saves nothing, signs that session out
/// again with its own token, and the command goes on signed out. A claim that saves whatever its
/// poll returns signs the account back in behind the sign-out, and the command that started it
/// then uses the token.
#[test]
fn signing_out_cancels_a_claim_whose_approval_arrives_afterward() {
    for (args, code) in [(["share", "list"], 5), (["login", "--complete"], 8)] {
        let release = Arc::new(AtomicBool::new(false));
        let hub = Hub::new(vec![
            Reply::Json(json!({"state":"SYNTHETIC-state",
                "url":"https://hub.example.test/auth/cli", "expires_in":600})),
            Reply::Held(Arc::clone(&release), Box::new(Reply::Json(session()))),
            Reply::Json(json!({})),
        ]);
        let lab = Lab::new(&hub.base);
        lab.create_locks(&hub.base);
        lab.handoff(&hub.base);
        let claiming = lab.spawn(&hub.base, &args);
        hub.wait_for(2);
        let logout = lab.run(&hub.base, &[], &["logout", "--all"], None);
        assert_command_output(&logout, &[], "logout", 0);
        assert!(!lab.pending_path(&hub.base).exists());
        release.store(true, Ordering::Release);
        let output = wait_bounded(claiming);
        assert_command_output(&output, &[], args[0], code);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("`agit logout` cancelled it"), "{stderr}");
        assert!(!lab.credential_path(&hub.base).exists());
        assert!(!lab.pending_path(&hub.base).exists());
        let requests = hub.finish();
        assert_eq!(requests.len(), 3, "{args:?}: {requests:?}");
        assert_request(
            &requests[1],
            "/api/auth/cli/poll",
            json!({"state":"SYNTHETIC-state"}),
        );
        assert_eq!(requests[2].method, "POST");
        assert_eq!(requests[2].target, "/api/auth/logout");
        assert_eq!(requests[2].authorization, format!("Bearer {ACCESS}"));
    }
}

/// A completion waits for the claim lock even when no record names its request, because the
/// process holding the lock may have received the session and not saved it yet; it then reports
/// the sign-in that process finished. The first completion is held between receiving the session
/// and saving it by the credential mutation lock. A completion that polls without waiting for
/// the lock hears from the Hub that the request was used and reports a sign-in that is about to
/// succeed as no longer valid.
#[test]
fn a_completion_waits_for_a_claim_still_saving_its_session() {
    let hub = Hub::new(vec![Reply::Json(session()), Reply::ExpiredToken]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    let mutation = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lab.store.join("credentials.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&mutation).unwrap();
    let args = ["login", "--complete", "SYNTHETIC-state"];
    let first = lab.spawn(&hub.base, &args);
    hub.wait_for(1);
    let mut second = lab.spawn(&hub.base, &args);
    thread::sleep(Duration::from_secs(2));
    let waited = second.try_wait().unwrap().is_none();
    fs2::FileExt::unlock(&mutation).unwrap();
    let first = wait_bounded(first);
    let second = wait_bounded(second);
    assert!(waited, "{second:?}");
    assert_output(&first, &[], 0);
    assert_output(&second, &[], 0);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        text.contains("another agit process finished this sign-in"),
        "{text}"
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    let requests = hub.finish();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_request(
        &requests[0],
        "/api/auth/cli/poll",
        json!({"state":"SYNTHETIC-state"}),
    );
}

/// A new `agit login` waits for a claim of the recorded request that is in flight instead of
/// replacing the request under it, and then reports the sign-in that claim finished. A login that
/// records its own request while the claim waits for its poll answer makes the claim find the
/// request replaced and sign out the session the human approved.
#[test]
fn a_new_login_waits_for_a_claim_in_flight_instead_of_replacing_it() {
    let release = Arc::new(AtomicBool::new(false));
    let hub = Hub::new(vec![
        cli_session("SYNTHETIC-state", 600),
        Reply::Aside(Arc::clone(&release), Box::new(Reply::Json(session()))),
        cli_session("SYNTHETIC-second", 600),
        Reply::Json(json!({})),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let claiming = lab.spawn(&hub.base, &["login", "--complete"]);
    hub.wait_for(2);
    let mut login = lab.spawn(&hub.base, &["login"]);
    thread::sleep(Duration::from_secs(2));
    let waited = login.try_wait().unwrap().is_none();
    release.store(true, Ordering::Release);
    let claimed = wait_bounded(claiming);
    let login = wait_bounded(login);
    assert!(waited, "{login:?}");
    assert_output(&claimed, &[], 0);
    assert_output(&login, &[], 0);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&login.stdout),
        String::from_utf8_lossy(&login.stderr)
    );
    assert!(
        text.contains("another agit process finished this sign-in"),
        "{text}"
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    assert!(!lab.pending_path(&hub.base).exists());
    let requests = hub.finish();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_request(
        &requests[1],
        "/api/auth/cli/poll",
        json!({"state":"SYNTHETIC-state"}),
    );
}

/// A new request replaces the recorded one only between two polls of it: a login that finds the
/// earlier request still waiting and asks the Hub for its own, while a claim of the earlier one
/// polls, records its request only once that claim committed. A login that records it as soon
/// as the Hub answers makes the claim find the request replaced and sign out the session the
/// human approved.
#[test]
fn a_new_request_replaces_the_recorded_one_only_between_polls() {
    let (created, approved) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let hub = Hub::new(vec![
        cli_session("SYNTHETIC-state", 600),
        Reply::Json(json!({"status":"pending"})),
        Reply::Aside(
            Arc::clone(&created),
            Box::new(cli_session("SYNTHETIC-second", 600)),
        ),
        Reply::Aside(Arc::clone(&approved), Box::new(Reply::Json(session()))),
        Reply::Json(json!({})),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let mut login = lab.spawn(&hub.base, &["login"]);
    hub.wait_for(3);
    let claiming = lab.spawn(&hub.base, &["login", "--complete"]);
    hub.wait_for(4);
    created.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && login.try_wait().unwrap().is_none() {
        thread::sleep(Duration::from_millis(10));
    }
    approved.store(true, Ordering::Release);
    let claimed = wait_bounded(claiming);
    let login = wait_bounded(login);
    assert_output(&claimed, &[], 0);
    assert_output(&login, &[], 8);
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    let requests = hub.finish();
    assert_eq!(requests.len(), 4, "{requests:?}");
}

/// A sign-out forgets the waiting request before it asks the Hub to revoke anything, so an
/// approval that arrives while the revoke is in flight is cancelled instead of saved. A sign-out
/// that revokes first leaves the request claimable meanwhile: the claim saves the session and
/// reports a sign-in, and the removal that follows deletes a session the sign-out never revoked.
/// The Hub answers nothing while the poll is held, so the sign-out's revoke waits behind it and
/// only a request forgotten before that revoke disappears in time.
#[test]
fn signing_out_forgets_a_waiting_request_before_it_revokes() {
    for args in [&["logout"][..], &["logout", "--all"][..]] {
        let release = Arc::new(AtomicBool::new(false));
        let hub = Hub::new(vec![
            cli_session("SYNTHETIC-state", 600),
            Reply::Held(Arc::clone(&release), Box::new(Reply::Json(session()))),
            Reply::Json(json!({})),
            Reply::Json(json!({})),
        ]);
        let lab = Lab::new(&hub.base);
        lab.create_locks(&hub.base);
        lab.handoff(&hub.base);
        lab.seed(&hub.base);
        let claiming = lab.spawn(&hub.base, &["login", "--complete"]);
        hub.wait_for(2);
        let logout = lab.spawn(&hub.base, args);
        wait_until("the sign-out forgets the waiting request", || {
            !lab.pending_path(&hub.base).exists()
        });
        release.store(true, Ordering::Release);
        let logout = wait_bounded(logout);
        let claimed = wait_bounded(claiming);
        assert_command_output(&logout, &[], "logout", 0);
        assert_output(&claimed, &[], 8);
        let stderr = String::from_utf8_lossy(&claimed.stderr);
        assert!(stderr.contains("`agit logout` cancelled it"), "{stderr}");
        assert!(!lab.credential_path(&hub.base).exists());
        let requests = hub.finish();
        assert_eq!(requests.len(), 4, "{args:?}: {requests:?}");
        let mut revoked = Vec::new();
        for request in &requests[2..] {
            assert_eq!(request.method, "POST");
            assert_eq!(request.target, "/api/auth/logout");
            revoked.push(request.authorization.clone());
        }
        revoked.sort();
        assert_eq!(
            revoked,
            [format!("Bearer {ACCESS}"), format!("Bearer {OLD_ACCESS}")],
            "{args:?}"
        );
    }
}

/// A sign-out revokes every session it removes, including one another sign-in saved after the
/// sign-out read the credentials it revokes: the removal returns what it removed, read under the
/// lock every save takes. A sign-out that revokes only what it read first deletes such a session
/// locally and leaves it alive on the Hub until it expires.
#[test]
fn signing_out_revokes_a_session_saved_while_it_revoked() {
    for args in [&["logout"][..], &["logout", "--all"][..]] {
        let release = Arc::new(AtomicBool::new(false));
        let hub = Hub::new(vec![
            Reply::Aside(Arc::clone(&release), Box::new(Reply::Json(json!({})))),
            Reply::Json(session()),
            Reply::Json(json!({})),
        ]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        let logout = lab.spawn(&hub.base, args);
        hub.wait_for(1);
        let signed_in = lab.run(
            &hub.base,
            &[],
            &["login", "--complete", "SYNTHETIC-state"],
            None,
        );
        assert_output(&signed_in, &[], 0);
        release.store(true, Ordering::Release);
        let logout = wait_bounded(logout);
        assert_command_output(&logout, &[], "logout", 0);
        assert!(!lab.credential_path(&hub.base).exists());
        let requests = hub.finish();
        assert_eq!(requests.len(), 3, "{args:?}: {requests:?}");
        assert_request(
            &requests[1],
            "/api/auth/cli/poll",
            json!({"state":"SYNTHETIC-state"}),
        );
        for (request, token) in [(&requests[0], OLD_ACCESS), (&requests[2], ACCESS)] {
            assert_eq!(request.method, "POST");
            assert_eq!(request.target, "/api/auth/logout");
            assert_eq!(request.authorization, format!("Bearer {token}"), "{args:?}");
        }
    }
}

/// A request that expires locally while a claim of it waits for the poll answer stays recorded
/// until that claim commits, so an approval the Hub handed out before its own expiry is saved. A
/// command that forgets the expired record without the claim lock withdraws the claim, which then
/// signs the approved session out again and blames a sign-out nobody ran.
#[test]
fn an_expired_request_is_not_forgotten_under_a_claim_in_flight() {
    let release = Arc::new(AtomicBool::new(false));
    let hub = Hub::new(vec![
        cli_session("SYNTHETIC-state", 3),
        Reply::Held(Arc::clone(&release), Box::new(Reply::Json(session()))),
        Reply::Json(json!({})),
    ]);
    let lab = Lab::new(&hub.base);
    lab.create_locks(&hub.base);
    lab.handoff(&hub.base);
    let expired = Instant::now() + Duration::from_millis(3500);
    let claiming = lab.spawn(&hub.base, &["login", "--complete"]);
    hub.wait_for(2);
    thread::sleep(expired.saturating_duration_since(Instant::now()));
    let check = lab.run(&hub.base, &[], &["whoami", "--check"], None);
    assert_command_output(&check, &[], "whoami", 5);
    release.store(true, Ordering::Release);
    let claimed = wait_bounded(claiming);
    assert_output(&claimed, &[], 0);
    let saved: Value =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    assert!(!lab.pending_path(&hub.base).exists());
    let requests = hub.finish();
    assert_eq!(requests.len(), 2, "{requests:?}");
}

fn cli_session(state: &str, expires_in: u64) -> Reply {
    Reply::Json(json!({"state":state,
        "url":"https://hub.example.test/auth/cli", "expires_in":expires_in}))
}

/// Wait until `condition` holds, failing the test if it does not within a bound.
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_bounded(mut command: Command) -> Output {
    wait_bounded(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

fn wait_bounded(mut child: Child) -> Output {
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

#[cfg(unix)]
#[test]
fn terminal_enter_uses_browser_default_and_explicit_choices_select_their_flow() {
    for (input, target) in [
        ("\r", "/api/auth/cli/session"),
        ("1\r", "/api/auth/cli/session"),
        ("2\r", "/api/auth/device/code"),
    ] {
        let hub = Hub::new(vec![Reply::Status(503)]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        let before = lab.state();
        let (status, output) = terminal_login(&lab, &hub.base, input);
        assert_eq!(status.exit_code(), 6, "{output}");
        assert!(output.contains("press Enter"), "{output}");
        assert!(
            !output.contains("needs an interactive terminal"),
            "{output}"
        );
        assert_eq!(lab.state(), before);
        let requests = hub.finish();
        assert_eq!(requests.len(), 1, "{output}");
        assert_request(&requests[0], target, json!({}));
    }
}

#[cfg(unix)]
#[test]
fn invalid_terminal_choice_can_be_corrected_without_restarting_login() {
    for (input, target) in [
        ("invalid\r\r", "/api/auth/cli/session"),
        ("invalid\r2\r", "/api/auth/device/code"),
    ] {
        let hub = Hub::new(vec![Reply::Status(503)]);
        let lab = Lab::new(&hub.base);
        lab.seed(&hub.base);
        let before = lab.state();
        let (status, output) = terminal_login(&lab, &hub.base, input);
        assert_eq!(status.exit_code(), 6, "{output}");
        assert!(
            output.contains("choose 1 for browser sign-in or 2 for a device code"),
            "{output}"
        );
        assert!(
            !output.contains("needs an interactive terminal"),
            "{output}"
        );
        assert_eq!(lab.state(), before);
        let requests = hub.finish();
        assert_eq!(requests.len(), 1, "{output}");
        assert_request(&requests[0], target, json!({}));
    }
}

#[cfg(unix)]
#[test]
fn rc_first_sign_in_persists_credentials_before_background_enrollment() {
    #[path = "support/startup_cache.rs"]
    mod startup_cache;
    let mut login = session();
    login["account_id"] = json!("00000000-0000-4000-8000-000000000001");
    let hub = Hub::new(vec![
        device(120),
        Reply::Json(login),
        Reply::Status(503),
        Reply::Status(503),
        Reply::Status(503),
    ]);
    let lab = Lab::new(&hub.base);
    startup_cache::seed(&lab.store);
    let (status, output) = terminal_command(&lab, &hub.base, "2\r", &["rc", "start", "--detach"]);
    let stop = || lab.run(&hub.base, &[], &["rc", "stop"], None);
    if status.exit_code() != 0 {
        let _ = stop();
        panic!("{output}");
    }
    let saved: HubCredential =
        serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
    assert_eq!(saved.access_token, ACCESS);
    thread::sleep(Duration::from_millis(500));
    assert!(stop().status.success());
    let requests = hub.finish();
    assert_eq!(requests.last().unwrap().target, "/api/peer/devices");
    assert_eq!(
        requests.last().unwrap().authorization,
        format!("Bearer {ACCESS}")
    );
}

#[cfg(unix)]
fn terminal_login(lab: &Lab, base: &str, input: &str) -> (portable_pty::ExitStatus, String) {
    terminal_command(lab, base, input, &["login"])
}

#[cfg(unix)]
fn terminal_command(
    lab: &Lab,
    base: &str,
    input: &str,
    args: &[&str],
) -> (portable_pty::ExitStatus, String) {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    command.args(args);
    command.env_clear();
    command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.env("HOME", &lab.home);
    command.env("AGIT_HOME", &lab.store);
    command.env("AGIT_HUB_URL", base);
    command.env("AGIT_SECRETS_KEYSTORE", "file");
    command.env("GIT_CONFIG_GLOBAL", lab.home.join("absent-gitconfig"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("NO_COLOR", "1");
    command.env("CI", "1");
    command.env("TERM", "xterm-256color");
    command.cwd(&lab.work);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _master = pair.master;
        let mut buffer = [0; 4096];
        while let Ok(size) = reader.read(&mut buffer) {
            if size == 0 || sender.send(buffer[..size].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut captured = Vec::new();
    let mut sent = false;
    loop {
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(10)) {
            captured.extend_from_slice(&bytes);
        }
        let output = String::from_utf8_lossy(&captured);
        if !sent && output.contains("choice") && output.contains(": ") {
            writer.write_all(input.as_bytes()).unwrap();
            writer.flush().unwrap();
            sent = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            while let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20)) {
                captured.extend_from_slice(&bytes);
            }
            return (status, String::from_utf8(captured).unwrap());
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("login did not respond to {input:?}: {output}");
        }
    }
}
