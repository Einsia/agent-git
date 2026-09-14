//! Organization search needs a server-confirmed corpus and never falls back to all repositories.

use agit::infra::credentials::{HubCredential, save_at};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ACCESS: &str = "SYNTHETIC-org-scope-access";
const REFRESH: &str = "SYNTHETIC-org-scope-refresh";
const HIT: &str = "SYNTHETIC-org-scope-hit";

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
                    account_id: None,
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

    fn run(&self, base: &str, args: &[&str]) -> Output {
        run_bounded(self.command(base, args))
    }

    fn mcp(&self, base: &str, arguments: Value) -> Output {
        let mut input = tempfile::tempfile().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"search", "arguments":arguments}})
        )
        .unwrap();
        input.rewind().unwrap();
        let mut command = self.command(base, &["mcp"]);
        command.stdin(Stdio::from(input));
        run_bounded(command)
    }

    fn command(&self, base: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
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
            .stdin(Stdio::null())
            .args(args);
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

#[test]
fn org_scope_is_sent_once_and_confirmed_in_every_output_format() {
    for kind in ["sessions", "agents"] {
        for (mode, mut args) in modes() {
            let hub = Hub::new(200, page(kind, Some(json!("org"))));
            let lab = Lab::new(&hub.base, true);
            let before = lab.state();
            args.extend([
                "search",
                "needle owner:acme",
                "--scope",
                "org",
                "--type",
                kind,
                "--sort",
                "recent",
                "--page",
                "2",
                "-n",
                "1",
            ]);
            if mode.starts_with("json") {
                args.push("--mcp");
            }
            let output = lab.run(&hub.base, &args);
            let result = match assert_output(&output, mode, 0) {
                Some(value) => value["result"]["value"].clone(),
                None => {
                    assert!(output.stderr.is_empty(), "{output:?}");
                    serde_json::from_slice(&output.stdout).unwrap()
                }
            };
            assert_eq!(result["applied_scope"], "org");
            assert_eq!(result["query"], "needle owner:acme");
            assert_eq!(result["total"], 4);
            assert_eq!(result["page"], 2);
            assert_eq!(result["per"], 1);
            assert_eq!(result["has_more"], true);
            assert_eq!(result["incomplete"], true);
            assert_eq!(result["unknown"], json!(["synthetic-unknown"]));
            assert!(result.to_string().contains(HIT));
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "GET");
            assert_eq!(
                requests[0].target,
                format!(
                    "/api/search/{kind}?q=needle%20owner%3Aacme&scope=org&sort=recent&page=2&per=1"
                )
            );
            assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
            assert!(requests[0].body.is_empty());
        }
    }
}

#[test]
fn missing_or_mismatched_scope_never_displays_unconfirmed_hits() {
    for applied in [None, Some(json!("public")), Some(Value::Null)] {
        for kind in ["sessions", "agents"] {
            for (mode, mut args) in modes() {
                let hub = Hub::new(200, page(kind, applied.clone()));
                let lab = Lab::new(&hub.base, true);
                let before = lab.state();
                args.extend([
                    "search", "needle", "--scope", "org", "--type", kind, "--mcp",
                ]);
                let output = lab.run(&hub.base, &args);
                assert_output(&output, mode, 4);
                let all = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(!all.contains(HIT));
                assert!(all.contains("did not confirm --scope org"));
                assert_eq!(lab.state(), before);
                assert_eq!(hub.finish().len(), 1);
            }
        }
    }
}

#[test]
fn org_scope_keeps_auth_precedence_and_rejects_unsupported_types_without_network() {
    let hub = Hub::new(500, json!({"error":"unexpected request"}));
    let lab = Lab::new(&hub.base, true);
    let before = lab.state();
    for extra in [
        vec!["--counts"],
        vec!["--type", "prs"],
        vec!["--type", "people"],
    ] {
        let mut args = vec!["--json", "search", "needle", "--scope", "org"];
        args.extend(extra);
        assert_output(&lab.run(&hub.base, &args), "json2", 2);
        assert_eq!(lab.state(), before);
    }
    let anonymous = Lab::new(&hub.base, false);
    let before = anonymous.state();
    for (mode, mut args) in modes() {
        args.extend(["search", "needle", "--scope", "org"]);
        assert_output(&anonymous.run(&hub.base, &args), mode, 5);
        assert_eq!(anonymous.state(), before);
    }
    assert!(hub.finish().is_empty());
}

#[test]
fn org_scope_request_failures_keep_typed_http_categories() {
    for (status, code) in [(401, 5), (500, 6)] {
        for (mode, mut args) in modes() {
            let hub = Hub::new(
                status,
                json!({"error":"HTTP 401 (synthetic body)", "applied_scope":"org"}),
            );
            let lab = Lab::new(&hub.base, true);
            let before = lab.state();
            args.extend(["search", "needle", "--scope", "org"]);
            assert_output(&lab.run(&hub.base, &args), mode, code);
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].target,
                "/api/search/sessions?q=needle&scope=org&per=10"
            );
        }
    }
}

#[test]
fn organization_batch_keeps_saved_filters_and_requires_each_acknowledgement() {
    for applied in [Some(json!("org")), None] {
        let mut response = page("sessions", applied.clone());
        response["applied_filters"] = json!({
            "author":"alice", "since":"2026-09-01T00:00:00Z",
            "before":"2026-09-02T00:00:00Z"
        });
        let hub = Hub::new(200, response);
        let lab = Lab::new(&hub.base, true);
        let before = lab.state();
        let output = lab.run(
            &hub.base,
            &[
                "--json",
                "search",
                "--query",
                "first",
                "--query",
                "second",
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
            ],
        );
        assert_eq!(
            output.status.code(),
            Some(if applied.is_some() { 0 } else { 1 })
        );
        assert!(output.stderr.is_empty(), "{output:?}");
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        let value = &envelope["result"]["value"];
        assert_eq!(value["batch"], true);
        let results = value["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        for (index, query) in ["first", "second"].iter().enumerate() {
            let row = &results[index];
            assert_eq!(row["query"], *query);
            assert_eq!(row["ok"], applied.is_some());
            if applied.is_some() {
                assert_eq!(row["result"]["query"], *query);
                assert_eq!(row["result"]["applied_scope"], "org");
                assert_eq!(row["result"]["applied_filters"]["author"], "alice");
                assert_eq!(row["result"]["page"], 2);
                assert_eq!(row["result"]["has_more"], true);
                assert_eq!(row["result"]["incomplete"], true);
            } else {
                assert!(
                    row["error"]
                        .as_str()
                        .unwrap()
                        .contains("did not confirm --scope org")
                );
                assert!(row.get("result").is_none());
            }
        }
        for secret in [ACCESS, REFRESH] {
            assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        }
        if applied.is_none() {
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
                    "/api/search/sessions?q={query}&scope=org&page=2&per=1&author=alice&since=2026-09-01T00%3A00%3A00Z&before=2026-09-02T00%3A00%3A00Z"
                )
            );
            assert!(request.body.is_empty());
        }
    }
}

#[test]
fn org_only_query_is_sent_empty_in_every_output_format() {
    for kind in ["sessions", "agents"] {
        for (mode, mut args) in modes() {
            let hub = Hub::new(200, page(kind, Some(json!("org"))));
            let lab = Lab::new(&hub.base, true);
            let before = lab.state();
            args.extend([
                "search", "--scope", "org", "--type", kind, "--page", "2", "--limit", "1", "--mcp",
            ]);
            let output = lab.run(&hub.base, &args);
            let result = match assert_output(&output, mode, 0) {
                Some(value) => value["result"]["value"].clone(),
                None => serde_json::from_slice(&output.stdout).unwrap(),
            };
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(result["query"], "");
            assert_eq!(result["applied_scope"], "org");
            assert!(result.to_string().contains(HIT));
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "GET");
            assert_eq!(
                requests[0].target,
                format!("/api/search/{kind}?q=&scope=org&page=2&per=1")
            );
            assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
            assert!(requests[0].body.is_empty());
        }
    }
}

#[test]
fn org_only_mcp_requires_the_empty_query_corpus_acknowledgement() {
    for kind in ["sessions", "agents"] {
        for confirmed in [true, false] {
            let hub = Hub::new(200, page(kind, confirmed.then(|| json!("org"))));
            let lab = Lab::new(&hub.base, true);
            let before = lab.state();
            let output = lab.mcp(
                &hub.base,
                json!({"scope":"org", "type":kind, "page":2, "limit":1}),
            );
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(reply["id"], 1);
            assert_eq!(reply["result"]["isError"], !confirmed);
            let text = reply["result"]["content"][0]["text"].as_str().unwrap();
            if confirmed {
                let result: Value = serde_json::from_str(text).unwrap();
                assert_eq!(result["query"], "");
                assert_eq!(result["applied_scope"], "org");
                assert!(text.contains(HIT));
            } else {
                assert!(text.contains("(exit 4)"), "{reply}");
                assert!(text.contains("did not confirm --scope org"), "{reply}");
                assert!(!text.contains(HIT), "{reply}");
            }
            for secret in [ACCESS, REFRESH] {
                assert!(!text.contains(secret));
            }
            assert_eq!(lab.state(), before);
            let requests = hub.finish();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "GET");
            assert_eq!(
                requests[0].target,
                format!("/api/search/{kind}?q=&scope=org&page=2&per=1")
            );
            assert_eq!(requests[0].authorization, format!("Bearer {ACCESS}"));
            assert!(requests[0].body.is_empty());
        }
    }
}

#[test]
fn org_only_auth_and_explicit_blank_batches_fail_before_http() {
    let hub = Hub::new(500, json!({"error":"unexpected request"}));
    let anonymous = Lab::new(&hub.base, false);
    let before = anonymous.state();
    for (mode, mut args) in modes() {
        args.extend(["search", "--scope", "org"]);
        assert_output(&anonymous.run(&hub.base, &args), mode, 5);
        assert_eq!(anonymous.state(), before);
    }
    let authenticated = Lab::new(&hub.base, true);
    for lab in [&anonymous, &authenticated] {
        let before = lab.state();
        for blank in ["", "  "] {
            let output = lab.run(
                &hub.base,
                &[
                    "--json",
                    "--json-version",
                    "2",
                    "search",
                    "--query",
                    "needle",
                    "--query",
                    blank,
                    "--scope",
                    "org",
                ],
            );
            assert_output(&output, "json2", 2);
            assert_eq!(lab.state(), before);
        }
    }
    for (lab, arguments, code) in [
        (&anonymous, json!({"scope":"org"}), 5),
        (
            &anonymous,
            json!({"scope":"org", "queries":["needle", ""]}),
            2,
        ),
        (
            &authenticated,
            json!({"scope":"org", "queries":["needle", ""]}),
            2,
        ),
    ] {
        let before = lab.state();
        let output = lab.mcp(&hub.base, arguments);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("(exit {code})")), "{reply}");
        for hidden in [ACCESS, REFRESH, HIT] {
            assert!(!text.contains(hidden));
        }
        assert_eq!(lab.state(), before);
    }
    assert!(hub.finish().is_empty());
}
