//! Code-origin search refuses unproved scopes before returning any candidate content.

use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ACCESS: &str = "SYNTHETIC-here-scope-access";
const REFRESH: &str = "SYNTHETIC-here-scope-refresh";
const HIT: &str = "SYNTHETIC-here-scope-hit";

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
    fn new(status: u16, body: Value) -> Self {
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
    fn new(base: &str, authenticated: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        #[cfg(windows)]
        let path = {
            let path = path.to_str().unwrap();
            if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
                PathBuf::from(format!(r"\\{share}"))
            } else {
                PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
            }
        };
        let lab = Self {
            home: path.join("home"),
            store: path.join("agit"),
            work: path.join("work"),
            root,
        };
        fs::create_dir_all(&lab.home).unwrap();
        fs::create_dir_all(&lab.work).unwrap();
        if authenticated {
            save_at(
                &lab.store.join("credentials").join(format!(
                    "{}.json",
                    agit::infra::config::hub_host_key(base).unwrap()
                )),
                &HubCredential {
                    username: "saved-display".into(),
                    email: None,
                    hub: Some(base.into()),
                    access_token: ACCESS.into(),
                    refresh_token: REFRESH.into(),
                    access_expires_at: "2099-01-01T00:00:00Z".into(),
                    refresh_expires_at: "2000-01-01T00:00:00Z".into(),
                },
            )
            .unwrap();
        }
        let warm = lab.run(base, &["config", "hub.url"]);
        assert!(warm.status.success(), "{warm:?}");
        lab
    }

    fn command(&self, program: impl AsRef<OsStr>, base: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_SESSION", "unselected/agent@branch")
            .env("AGIT_HUB_URL", base)
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
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

    fn run(&self, base: &str, args: &[&str]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"), base);
        command.args(args);
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

fn run_bounded(mut command: Command) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
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
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.kill();
            panic!(
                "command deadline elapsed: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(unix, feature = "rc"))]
fn run_tty(lab: &Lab, base: &str, args: &[&str]) -> (u32, String) {
    let template = lab.command(env!("CARGO_BIN_EXE_agit"), base);
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
    command.env_clear();
    for (name, value) in template.get_envs() {
        if name != "CI"
            && let Some(value) = value
        {
            command.env(name, value);
        }
    }
    command.env("TERM", "xterm-256color");
    command.cwd(&lab.work);
    command.args(args);
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let reader = pty.master.try_clone_reader().unwrap();
    let output_reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = reader.take(65537).read_to_end(&mut output);
        match result {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => return Err(error),
        }
        Ok(output)
    });
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(format!("could not poll search: {error}")),
        }
        if Instant::now() >= deadline {
            break Err("interactive search deadline elapsed".to_owned());
        }
        thread::sleep(Duration::from_millis(10));
    };
    if status.is_err() {
        if let Some(pid) = child.process_id() {
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(pty.master);
    let output = output_reader.join().unwrap().unwrap();
    assert!(
        output.len() <= 65536,
        "interactive search output is unbounded"
    );
    let output = String::from_utf8(output).unwrap();
    let status = status.unwrap_or_else(|error| panic!("{error}: {output}"));
    for secret in [ACCESS, REFRESH, "SYNTHETIC-secret"] {
        assert!(!output.contains(secret), "credential escaped");
    }
    (status.exit_code(), output)
}

#[cfg(all(unix, feature = "rc"))]
#[test]
fn interactive_here_validation_does_not_probe_updates_or_write_a_cache() {
    for (authenticated, origin, extra, expected) in [
        (true, None, None, 4),
        (
            true,
            Some("https://SYNTHETIC-secret@example.test/repo"),
            None,
            4,
        ),
        (false, None, None, 5),
        (true, Some(ORIGIN), Some("--counts"), 2),
    ] {
        let hub = Hub::new(500, json!({"error":"unexpected request"}));
        let lab = Lab::new(&hub.base, authenticated);
        if let Some(origin) = origin {
            seed(&lab, origin);
        }
        assert!(!lab.store.join("cli-update.json").exists());
        let before = lab.state();
        let mut args = vec!["search", "--here"];
        if let Some(extra) = extra {
            args.push(extra);
        }
        let (code, output) = run_tty(&lab, &hub.base, &args);
        assert_eq!(code, expected, "{output}");
        assert!(!output.contains(HIT));
        assert_eq!(lab.state(), before);
        assert!(hub.finish().is_empty());
    }

    let hub = Hub::new(200, here_page(Some(ORIGIN)));
    let lab = Lab::new(&hub.base, true);
    seed(&lab, ORIGIN);
    assert!(!lab.store.join("cli-update.json").exists());
    let before = lab.state();
    let (code, output) = run_tty(&lab, &hub.base, &["search", "--here"]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains(HIT), "{output}");
    assert!(output.contains("scope: exact code Git origin"), "{output}");
    assert_eq!(lab.state(), before);
    let requests = hub.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
    assert!(requests[0].body.is_empty());
    assert_eq!(
        requests[0].target,
        format!(
            "/api/search/sessions?q=&code_origin={}&per=10",
            encode(ORIGIN)
        )
    );
}

fn modes() -> [(&'static str, Vec<&'static str>); 4] {
    [
        ("pipe", vec![]),
        ("quiet-pipe", vec!["--quiet"]),
        ("json1", vec!["--json", "--json-version", "1"]),
        ("json2", vec!["--json", "--json-version", "2"]),
    ]
}

fn page(kind: &str, applied: Option<Value>) -> Value {
    let hit = if kind == "agents" {
        json!({"owner":"acme", "name":HIT, "slug":format!("acme/{HIT}"), "visibility":"private", "category":"general", "fork":false, "size_bytes":1, "updated_at":"2026-09-01T00:00:00Z", "url":"https://example.test/selected"})
    } else {
        json!({"agent":format!("acme/{HIT}"), "session_id":"agit-1111111111111111111111111111111111111111", "excerpt":HIT, "url":"https://example.test/selected"})
    };
    let mut page = json!({"type":kind,"total":4,"page":2,"per":1,"items":[hit],"incomplete":true,"unknown":["synthetic-unknown"]});
    if let Some(applied) = applied {
        page["applied_scope"] = applied;
    }
    page
}

fn assert_output(output: &Output, mode: &str, code: i32) -> Option<Value> {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in [ACCESS, REFRESH] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "credential escaped"
        );
    }
    if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        if code != 0 {
            assert_eq!(value["result"]["format"], "empty");
        }
        Some(value)
    } else {
        None
    }
}

const ORIGIN: &str = "ssh://git@Example.test:2222/team/Repo.git";

fn git(lab: &Lab, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", lab.home.join("empty-gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-C", lab.work.to_str().unwrap()])
        .args(args);
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn seed(lab: &Lab, origin: &str) {
    git(lab, &["init", "--initial-branch=main"]);
    git(lab, &["config", "remote.origin.url", origin]);
}

fn here_page(origin: Option<&str>) -> Value {
    let mut body = page("sessions", None);
    if let Some(origin) = origin {
        body["applied_filters"] = json!({"code_origin":origin});
    }
    body
}

#[test]
fn here_preserves_exact_origin_and_intersects_org_in_every_output_mode() {
    for origin in [
        ORIGIN,
        "git@Example.test:team/Repo.git",
        "https://Example.test:114/team/Repo.git",
    ] {
        for (mode, mut args) in modes() {
            let mut body = here_page(Some(origin));
            body["applied_scope"] = json!("org");
            let hub = Hub::new(200, body);
            let lab = Lab::new(&hub.base, true);
            seed(&lab, origin);
            let before = lab.state();
            args.extend([
                "search", "needle", "--here", "--scope", "org", "--page", "2", "-n", "1",
            ]);
            if mode.starts_with("json") {
                args.push("--mcp");
            }
            let output = lab.run(&hub.base, &args);
            if let Some(value) = assert_output(&output, mode, 0) {
                assert_eq!(
                    value["result"]["value"]["applied_filters"]["code_origin"],
                    origin
                );
                assert_eq!(value["result"]["value"]["applied_scope"], "org");
                assert_eq!(value["result"]["value"]["incomplete"], true);
            } else {
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["applied_filters"]["code_origin"], origin);
                assert_eq!(value["applied_scope"], "org");
            }
            assert!(String::from_utf8_lossy(&output.stdout).contains(HIT));
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].target,
                format!(
                    "/api/search/sessions?q=needle&code_origin={}&scope=org&page=2&per=1",
                    encode(origin)
                )
            );
            assert_eq!(requests[0].method, "GET");
            assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
            assert!(requests[0].body.is_empty());
        }
    }
}

#[test]
fn here_code_metadata_producer_probe() {
    let Some(destination) = std::env::var_os("AGIT_TEST_HERE_CODE_METADATA") else {
        return;
    };
    let code = agit::domain::meta::code_of(&std::env::current_dir().unwrap()).unwrap();
    fs::write(
        destination,
        serde_json::to_vec(&json!({"code": code})).unwrap(),
    )
    .unwrap();
}

#[test]
fn here_safe_rewrites_match_recorded_code_provenance_in_every_output_mode() {
    let configured = "https://Example.test/team/Repo.git";
    for (location, replacement, effective) in [
        (
            "--local",
            "ssh://git@Example.test:2222/",
            "ssh://git@Example.test:2222/team/Repo.git",
        ),
        (
            "--global",
            "reader@Example.test:",
            "reader@Example.test:team/Repo.git",
        ),
    ] {
        let hub = Hub::new(200, here_page(Some(effective)));
        let lab = Lab::new(&hub.base, true);
        seed(&lab, configured);
        git(&lab, &["config", "user.name", "Synthetic Here"]);
        git(&lab, &["config", "user.email", "here@example.test"]);
        git(
            &lab,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "Synthetic code",
            ],
        );
        git(
            &lab,
            &[
                "config",
                location,
                &format!("url.{replacement}.insteadOf"),
                "https://Example.test/",
            ],
        );
        assert_eq!(
            git(&lab, &["config", "--local", "--get", "remote.origin.url"]),
            format!("{configured}\n")
        );
        let destination = lab.root.path().join("recorded-meta.json");
        let mut producer = lab.command(std::env::current_exe().unwrap(), &hub.base);
        producer
            .env("AGIT_TEST_HERE_CODE_METADATA", &destination)
            .args([
                "--exact",
                "here_code_metadata_producer_probe",
                "--nocapture",
            ]);
        let output = run_bounded(producer);
        assert!(output.status.success(), "{output:?}");
        let recorded: Value = serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
        let code = recorded["code"].as_str().unwrap();
        let (recorded_origin, recorded_commit) = code.rsplit_once('@').unwrap();
        assert_eq!(recorded_origin, effective);
        assert_ne!(recorded_origin, configured);
        assert_eq!(
            recorded_commit,
            git(&lab, &["rev-parse", "--short", "HEAD"]).trim()
        );
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "needle", "--here", "--mcp"]);
            let output = lab.run(&hub.base, &args);
            if let Some(value) = assert_output(&output, mode, 0) {
                assert_eq!(
                    value["result"]["value"]["applied_filters"]["code_origin"],
                    recorded_origin
                );
            }
            assert!(String::from_utf8_lossy(&output.stdout).contains(HIT));
            assert_eq!(lab.state(), before);
        }
        let requests = hub.finish();
        assert_eq!(requests.len(), modes().len());
        for request in requests {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.target,
                format!(
                    "/api/search/sessions?q=needle&code_origin={}&per=10",
                    encode(recorded_origin)
                )
            );
            assert_eq!(request.authorization, format!("Bearer {ACCESS}"));
            assert!(request.body.is_empty());
        }
    }
}

#[test]
fn here_rewrite_safety_and_raw_uniqueness_are_checked_before_any_request() {
    let safe = "https://Example.test/team/Repo.git";
    let oversized = format!("https://example.test/{}", "a".repeat(4096));
    for (configured, effective, multiple) in [
        (safe, "https://SYNTHETIC-secret@example.test/repo", false),
        (safe, "ssh://git:SYNTHETIC-secret@example.test/repo", false),
        (safe, "https://example.test/repo?SYNTHETIC-secret", false),
        (safe, "https://example.test/repo#SYNTHETIC-secret", false),
        (safe, "file:///SYNTHETIC-secret/repo", false),
        (safe, oversized.as_str(), false),
        ("https://SYNTHETIC-secret@example.test/repo", ORIGIN, false),
        (safe, ORIGIN, true),
    ] {
        let hub = Hub::new(500, json!({"error":"unexpected"}));
        let lab = Lab::new(&hub.base, true);
        seed(&lab, configured);
        git(
            &lab,
            &["config", &format!("url.{effective}.insteadOf"), configured],
        );
        if multiple {
            git(&lab, &["config", "--add", "remote.origin.url", ORIGIN]);
        }
        assert_eq!(
            git(&lab, &["remote", "get-url", "origin"]),
            format!("{effective}\n")
        );
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "needle", "--here", "--scope", "mine"]);
            let output = lab.run(&hub.base, &args);
            assert_output(&output, mode, 4);
            assert!(!format!("{output:?}").contains("SYNTHETIC-secret"));
            assert_eq!(lab.state(), before);
        }
        assert!(hub.finish().is_empty());
    }
}

#[test]
fn here_ignores_system_rewrites_to_match_recorded_code_provenance() {
    let configured = "https://Example.test/team/Repo.git";
    for replacement in [ORIGIN, "https://SYNTHETIC-secret@example.test/repo"] {
        let hub = Hub::new(200, here_page(Some(configured)));
        let lab = Lab::new(&hub.base, true);
        seed(&lab, configured);
        git(&lab, &["config", "user.name", "Synthetic Here"]);
        git(&lab, &["config", "user.email", "here@example.test"]);
        git(
            &lab,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "Synthetic code",
            ],
        );
        let system = lab.home.join("system-gitconfig");
        git(
            &lab,
            &[
                "config",
                "--file",
                system.to_str().unwrap(),
                &format!("url.{replacement}.insteadOf"),
                configured,
            ],
        );
        let destination = lab.root.path().join("recorded-meta.json");
        let mut producer = lab.command(std::env::current_exe().unwrap(), &hub.base);
        producer
            .env_remove("GIT_CONFIG_NOSYSTEM")
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("AGIT_TEST_HERE_CODE_METADATA", &destination)
            .args([
                "--exact",
                "here_code_metadata_producer_probe",
                "--nocapture",
            ]);
        let output = run_bounded(producer);
        assert!(output.status.success(), "{output:?}");
        let recorded: Value = serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
        assert_eq!(
            recorded["code"]
                .as_str()
                .unwrap()
                .rsplit_once('@')
                .unwrap()
                .0,
            configured
        );
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "needle", "--here", "--mcp"]);
            let mut command = lab.command(env!("CARGO_BIN_EXE_agit"), &hub.base);
            command
                .env_remove("GIT_CONFIG_NOSYSTEM")
                .env("GIT_CONFIG_SYSTEM", &system)
                .args(args);
            let output = run_bounded(command);
            assert_output(&output, mode, 0);
            assert!(String::from_utf8_lossy(&output.stdout).contains(HIT));
            assert!(!format!("{output:?}").contains("SYNTHETIC-secret"));
            assert_eq!(lab.state(), before);
        }
        let requests = hub.finish();
        assert_eq!(requests.len(), modes().len());
        for request in requests {
            assert_eq!(
                request.target,
                format!(
                    "/api/search/sessions?q=needle&code_origin={}&per=10",
                    encode(configured)
                )
            );
        }
    }
}

#[test]
fn here_missing_or_different_acknowledgement_withholds_hits_and_counts() {
    for acknowledged in [
        None,
        Some("ssh://git@Example.test:22/team/Repo.git"),
        Some("ssh://other@Example.test:2222/team/Repo.git"),
    ] {
        for (mode, mut args) in modes() {
            let hub = Hub::new(200, here_page(acknowledged));
            let lab = Lab::new(&hub.base, true);
            seed(&lab, ORIGIN);
            let before = lab.state();
            args.extend(["search", "needle", "--here", "--mcp"]);
            let output = lab.run(&hub.base, &args);
            assert_output(&output, mode, 4);
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(all.contains("did not confirm --here"));
            assert!(!all.contains(HIT));
            assert_eq!(lab.state(), before);
            assert_eq!(hub.finish().len(), 1);
        }
    }
}

#[test]
fn here_refuses_missing_multiple_and_credential_bearing_origins_without_network_or_writes() {
    for origin in [
        None,
        Some(""),
        Some("https://SYNTHETIC-secret@example.test/repo"),
        Some("ssh://git:SYNTHETIC-secret@example.test/repo"),
        Some("https://example.test/repo?SYNTHETIC-secret"),
        Some("git@example.test:repo"),
    ] {
        let hub = Hub::new(500, json!({"error":"unexpected"}));
        let lab = Lab::new(&hub.base, true);
        if let Some(origin) = origin {
            if origin.is_empty() {
                git(&lab, &["init", "--initial-branch=main"]);
            } else {
                seed(&lab, origin);
            }
        }
        if origin == Some("git@example.test:repo") {
            git(&lab, &["config", "--add", "remote.origin.url", ORIGIN]);
        }
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "needle", "--here"]);
            let output = lab.run(&hub.base, &args);
            assert_output(&output, mode, 4);
            assert!(!format!("{output:?}").contains("SYNTHETIC-secret"));
            assert_eq!(lab.state(), before);
        }
        assert!(hub.finish().is_empty());
    }
}

#[test]
fn here_rejects_unsupported_categories_before_mine_identity_and_preserves_auth_precedence() {
    let hub = Hub::new(500, json!({"error":"unexpected"}));
    let lab = Lab::new(&hub.base, true);
    let before = lab.state();
    for extra in [
        vec!["--counts"],
        vec!["--type", "agents"],
        vec!["--type", "people"],
        vec!["--type", "prs"],
    ] {
        let mut args = vec!["--json", "search", "needle", "--here", "--scope", "mine"];
        args.extend(extra);
        assert_output(&lab.run(&hub.base, &args), "json2", 2);
        assert_eq!(lab.state(), before);
    }
    let anonymous = Lab::new(&hub.base, false);
    for (mode, mut args) in modes() {
        args.extend(["search", "needle", "--here"]);
        assert_output(&anonymous.run(&hub.base, &args), mode, 5);
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn here_uses_explicit_c_directory_instead_of_agent_session_or_another_remote() {
    let hub = Hub::new(200, here_page(Some(ORIGIN)));
    let lab = Lab::new(&hub.base, true);
    seed(&lab, ORIGIN);
    git(
        &lab,
        &["config", "remote.peer.url", "https://other.test/repo"],
    );
    let nested = lab.work.join("nested");
    fs::create_dir(&nested).unwrap();
    let before = lab.state();
    let output = lab.run(
        &hub.base,
        &[
            "-C",
            nested.to_str().unwrap(),
            "--json",
            "search",
            "needle",
            "--here",
            "--mcp",
        ],
    );
    assert_output(&output, "json2", 0);
    assert_eq!(lab.state(), before);
    assert_eq!(hub.finish().len(), 1);
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[test]
fn here_keeps_typed_auth_and_network_failures_without_replaying_the_request() {
    for (status, code) in [(401, 5), (500, 6)] {
        for (mode, mut args) in modes() {
            let hub = Hub::new(status, json!({"error":"HTTP 401 (synthetic body)"}));
            let lab = Lab::new(&hub.base, true);
            seed(&lab, ORIGIN);
            let before = lab.state();
            args.extend(["search", "needle", "--here"]);
            assert_output(&lab.run(&hub.base, &args), mode, code);
            assert_eq!(lab.state(), before);
            assert_eq!(hub.finish().len(), 1);
        }
    }
}

#[test]
fn here_batch_preserves_saved_filters_and_withholds_each_unconfirmed_page() {
    for acknowledged in [true, false] {
        for (mode, mut args) in modes() {
            let mut response = here_page(acknowledged.then_some(ORIGIN));
            response["applied_scope"] = json!("org");
            response["applied_filters"]["author"] = json!("alice");
            response["applied_filters"]["since"] = json!("2026-09-01T00:00:00Z");
            response["applied_filters"]["before"] = json!("2026-09-02T00:00:00Z");
            let hub = Hub::new(200, response);
            let lab = Lab::new(&hub.base, true);
            seed(&lab, ORIGIN);
            let before = lab.state();
            args.extend([
                "search",
                "--query",
                "first",
                "--query",
                "second",
                "--here",
                "--scope",
                "org",
                "--author",
                "Alice",
                "--since",
                "2026-09-01",
                "--before",
                "2026-09-02",
                "--page",
                "2",
                "--limit",
                "1",
            ]);
            let output = lab.run(&hub.base, &args);
            let code = if acknowledged { 0 } else { 1 };
            assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let value = if let Some(version) = mode.strip_prefix("json") {
                assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                assert_eq!(value["exit_code"], code);
                &value["result"]["value"]
            } else {
                &value
            };
            assert_eq!(value["batch"], true);
            let rows = value["results"].as_array().unwrap();
            assert_eq!(rows.len(), 2);
            for (row, query) in rows.iter().zip(["first", "second"]) {
                assert_eq!(row["query"], query);
                assert_eq!(row["ok"], acknowledged);
                if acknowledged {
                    assert_eq!(row["result"]["applied_filters"]["code_origin"], ORIGIN);
                    assert_eq!(row["result"]["applied_filters"]["author"], "alice");
                    assert_eq!(row["result"]["applied_scope"], "org");
                    assert_eq!(row["result"]["incomplete"], true);
                    assert_eq!(row["result"]["has_more"], true);
                } else {
                    assert!(row.get("result").is_none());
                    assert!(
                        row["error"]
                            .as_str()
                            .unwrap()
                            .contains("did not confirm --here")
                    );
                }
            }
            for value in [ACCESS, REFRESH] {
                assert!(!String::from_utf8_lossy(&output.stdout).contains(value));
            }
            if !acknowledged {
                assert!(!String::from_utf8_lossy(&output.stdout).contains(HIT));
            }
            assert_eq!(lab.state(), before);
            let mut requests = hub.finish();
            requests.sort_by(|a, b| a.target.cmp(&b.target));
            assert_eq!(requests.len(), 2);
            for (request, query) in requests.iter().zip(["first", "second"]) {
                assert_eq!(request.method, "GET");
                assert_eq!(request.authorization, format!("Bearer {ACCESS}"));
                assert_eq!(
                    request.target,
                    format!(
                        "/api/search/sessions?q={query}&code_origin={}&scope=org&page=2&per=1&author=alice&since=2026-09-01T00%3A00%3A00Z&before=2026-09-02T00%3A00%3A00Z",
                        encode(ORIGIN)
                    )
                );
                assert!(request.body.is_empty());
            }
        }
    }
}

#[test]
fn here_requires_the_requested_kind_even_when_origin_is_acknowledged() {
    let mut response = page("agents", None);
    response["applied_filters"] = json!({"code_origin":ORIGIN});
    assert!(serde_json::from_value::<agit::hub::AgentHit>(response["items"][0].clone()).is_ok());
    assert!(serde_json::from_value::<agit::hub::SearchHit>(response["items"][0].clone()).is_err());
    let hub = Hub::new(200, response);
    let lab = Lab::new(&hub.base, true);
    seed(&lab, ORIGIN);
    let before = lab.state();
    let output = lab.run(&hub.base, &["--json", "search", "needle", "--here"]);
    assert_output(&output, "json2", 4);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(HIT));
    assert_eq!(lab.state(), before);
    assert_eq!(hub.finish().len(), 1);
}

#[test]
fn here_filter_only_preserves_scope_and_explicit_empty_query_refusal() {
    for scope in [None, Some("org")] {
        for (mode, mut args) in modes() {
            let mut response = here_page(Some(ORIGIN));
            if let Some(scope) = scope {
                response["applied_scope"] = json!(scope);
            }
            let hub = Hub::new(200, response);
            let lab = Lab::new(&hub.base, true);
            seed(&lab, ORIGIN);
            let before = lab.state();
            args.extend(["search", "--here"]);
            if let Some(scope) = scope {
                args.extend(["--scope", scope]);
            }
            let output = lab.run(&hub.base, &args);
            let envelope = assert_output(&output, mode, 0);
            let value = envelope
                .map(|envelope| envelope["result"]["value"].clone())
                .unwrap_or_else(|| serde_json::from_slice(&output.stdout).unwrap());
            assert_eq!(value["query"], "");
            assert_eq!(value["applied_filters"]["code_origin"], ORIGIN);
            assert_eq!(value["incomplete"], true);
            assert!(String::from_utf8_lossy(&output.stdout).contains(HIT));
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            let scope = scope
                .map(|scope| format!("&scope={scope}"))
                .unwrap_or_default();
            assert_eq!(
                requests[0].target,
                format!(
                    "/api/search/sessions?q=&code_origin={}{scope}&per=10",
                    encode(ORIGIN)
                )
            );
            assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
            assert!(requests[0].body.is_empty());
        }
    }
    let hub = Hub::new(500, json!({"error":"unexpected"}));
    for authenticated in [false, true] {
        let lab = Lab::new(&hub.base, authenticated);
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "--here"]);
            assert_output(
                &lab.run(&hub.base, &args),
                mode,
                if authenticated { 4 } else { 5 },
            );
            for query in ["", " "] {
                let mut explicit = args.clone();
                explicit.extend(["--query", query]);
                assert_output(&lab.run(&hub.base, &explicit), mode, 2);
            }
            assert_eq!(lab.state(), before);
        }
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn mcp_here_filter_only_uses_the_same_validated_origin() {
    let hub = Hub::new(200, here_page(Some(ORIGIN)));
    let lab = Lab::new(&hub.base, true);
    seed(&lab, ORIGIN);
    let input = lab.root.path().join("mcp-input.jsonl");
    fs::write(
        &input,
        format!(
            "{}\n",
            json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"search", "arguments":{"here":true}}})
        ),
    )
    .unwrap();
    let before = lab.state();
    let mut command = lab.command(env!("CARGO_BIN_EXE_agit"), &hub.base);
    command.arg("mcp").stdin(fs::File::open(input).unwrap());
    let output = run_bounded(command);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["isError"], false);
    let result: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["query"], "");
    assert_eq!(result["applied_filters"]["code_origin"], ORIGIN);
    for secret in [ACCESS, REFRESH] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    }
    assert_eq!(lab.state(), before);
    let requests = hub.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].target,
        format!(
            "/api/search/sessions?q=&code_origin={}&per=10",
            encode(ORIGIN)
        )
    );
    assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
}

#[cfg(unix)]
#[test]
fn here_config_fifo_refuses_within_deadline_and_restored_origin_still_searches() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    for local in [true, false] {
        let hub = Hub::new(200, here_page(Some(ORIGIN)));
        let lab = Lab::new(&hub.base, true);
        seed(&lab, ORIGIN);
        let fifo = if local {
            lab.work.join(".git/config")
        } else {
            let fifo = lab.home.join("included-config");
            git(
                &lab,
                &[
                    "config",
                    "--file",
                    lab.home.join("empty-gitconfig").to_str().unwrap(),
                    "include.path",
                    fifo.to_str().unwrap(),
                ],
            );
            fs::write(&fifo, b"").unwrap();
            fifo
        };
        let original = fs::read(&fifo).unwrap();
        fs::remove_file(&fifo).unwrap();
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let identity = fs::symlink_metadata(&fifo).unwrap();
        assert!(identity.file_type().is_fifo());
        let before = lab.state();
        for (mode, mut args) in modes() {
            args.extend(["search", "--here"]);
            let started = Instant::now();
            let output = lab.run(&hub.base, &args);
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "{mode}: {output:?}"
            );
            assert_output(&output, mode, 4);
            assert!(!format!("{output:?}").contains(HIT));
            assert_eq!(lab.state(), before);
            let after = fs::symlink_metadata(&fifo).unwrap();
            assert!(after.file_type().is_fifo());
            assert_eq!((after.dev(), after.ino()), (identity.dev(), identity.ino()));
        }
        fs::remove_file(&fifo).unwrap();
        fs::write(&fifo, &original).unwrap();
        let before = lab.state();
        let output = lab.run(&hub.base, &["search", "--here", "--mcp"]);
        assert_output(&output, "pipe", 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains(HIT));
        assert_eq!(lab.state(), before);
        assert_eq!(fs::read(&fifo).unwrap(), original);
        let requests = hub.finish();
        assert_eq!(
            requests.len(),
            1,
            "failed origin queries must not reach the Hub"
        );
        assert_eq!(
            requests[0].target,
            format!(
                "/api/search/sessions?q=&code_origin={}&per=10",
                encode(ORIGIN)
            )
        );
        assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
    }
}
