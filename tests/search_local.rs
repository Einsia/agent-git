//! Offline search binds saved Git evidence and never consults Hub or native state.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Lab {
    temp: tempfile::TempDir,
    base: String,
    requests: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

#[path = "support/startup_cache.rs"]
mod startup_cache;

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let server = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        seen.fetch_add(1, Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let _ = stream.read(&mut [0; 4096]);
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(e) => panic!("loopback sentinel failed: {e}"),
                }
            }
        });
        for dir in ["home", "agit/credentials", "native", "work"] {
            std::fs::create_dir_all(temp.path().join(dir)).unwrap();
        }
        std::fs::write(
            temp.path()
                .join(format!("agit/credentials/127.0.0.1_{port}.json")),
            serde_json::to_vec(&json!({
                "username":"alice", "hub":base, "access_token":"fixture-only",
                "access_expires_at":"2001-01-01T00:00:00Z", "refresh_token":"fixture-only",
                "refresh_expires_at":"2001-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("native/unsettled.jsonl"),
            "native-only-needle\n",
        )
        .unwrap();
        Self {
            temp,
            base,
            requests,
            done,
            server: Some(server),
        }
    }

    fn command(&self, mode: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.current_dir(self.temp.path().join("work"))
            .env("HOME", self.temp.path().join("home"))
            .env("USERPROFILE", self.temp.path().join("home"))
            .env("AGIT_HOME", self.temp.path().join("agit"))
            .env("AGIT_HUB_URL", &self.base)
            .env("CODEX_HOME", self.temp.path().join("native"))
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        for key in [
            "AGIT_SESSION",
            "AGIT_SESSION_ID",
            "AGIT_MERGE_TX",
            "AGIT_RC",
            "AGIT_RUNTIME",
            "AGIT_ROLE",
            "AGIT_TURN_ID",
            "CLAUDECODE",
        ] {
            cmd.env_remove(key);
        }
        match mode {
            "quiet" => {
                cmd.arg("--quiet");
            }
            "json1" => {
                cmd.args(["--json", "--json-version", "1"]);
            }
            "json2" => {
                cmd.args(["--json", "--json-version", "2"]);
            }
            _ => {}
        }
        cmd
    }

    fn seed(&self, name: &str, layout: meta::LayoutVersion, contents: &[Value]) -> Repo {
        self.seed_runtime(name, layout, "claude-code", contents)
    }

    fn seed_runtime(
        &self,
        name: &str,
        layout: meta::LayoutVersion,
        runtime: &str,
        contents: &[Value],
    ) -> Repo {
        let path = self.temp.path().join("agit/repos/alice").join(name);
        let repo = Repo::init(&path).unwrap();
        let sid = format!("agit-{}", "a".repeat(40));
        let raw: String = contents.iter().map(|v| format!("{v}\n")).collect();
        let log = transcript::wrap_lines(&raw, runtime, &sid);
        let mut metadata = meta::Meta::new(sid, runtime.into(), "/synthetic".into());
        metadata.layout = layout;
        metadata.turn = Some(1);
        meta::write(&path, &metadata).unwrap();
        match layout {
            meta::LayoutVersion::V1 => storage::write_snapshot(&path, &log, "").unwrap(),
            meta::LayoutVersion::V0 => {
                std::fs::write(path.join(meta::LEGACY_LOG_FILE), &log).unwrap();
                std::fs::write(path.join(meta::LEGACY_VIEW_FILE), "").unwrap();
            }
        }
        repo.add_all().unwrap();
        repo.commit("saved search fixture").unwrap();
        repo
    }

    fn assert_offline(&self) {
        assert_eq!(
            self.requests.load(Ordering::SeqCst),
            0,
            "local mode must not send any request, including refresh"
        );
        assert!(!self.temp.path().join("agit/store").exists());
        assert!(!self.temp.path().join("agit/layout-v1.lock").exists());
    }

    fn commit_at(&self, repo: &Repo, second: u8) {
        repo.add_all().unwrap();
        let mut command = Command::new("git");
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let date = format!("2030-01-01T00:00:{second:02}Z");
        let output = command
            .arg("-C")
            .arg(repo.root())
            .args(["-c", "commit.gpgsign=false", "-c"])
            .arg(format!(
                "core.hooksPath={}",
                self.temp.path().join("absent-hooks").display()
            ))
            .args(["commit", "-m", "Save bounded search fixture"])
            .env("HOME", self.temp.path().join("home"))
            .env("USERPROFILE", self.temp.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temp.path().join("absent-gitconfig"),
            )
            .env("GIT_AUTHOR_NAME", "Search fixture")
            .env("GIT_AUTHOR_EMAIL", "search@example.invalid")
            .env("GIT_COMMITTER_NAME", "Search fixture")
            .env("GIT_COMMITTER_EMAIL", "search@example.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

fn prompt(text: &str) -> Value {
    json!({"type":"user", "message":{"role":"user", "content":text}})
}

fn inventory(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.insert(format!("{key}/"), vec![]);
                visit(root, &path, out);
            } else if kind.is_symlink() {
                out.insert(
                    key,
                    std::fs::read_link(path)
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                );
            } else {
                out.insert(key, std::fs::read(path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn value(output: &Output, mode: &str) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    if mode.starts_with("json") {
        assert_eq!(doc["ok"], true);
        doc["result"]["value"].clone()
    } else {
        doc
    }
}

fn mcp(lab: &Lab, requests: &[Value], session: Option<&str>, quiet: bool) -> Vec<Value> {
    let mut input = tempfile::tempfile().unwrap();
    for request in requests {
        writeln!(input, "{request}").unwrap();
    }
    use std::io::Seek as _;
    input.rewind().unwrap();
    let mut command = lab.command(if quiet { "quiet" } else { "plain" });
    command.arg("mcp").stdin(input);
    if let Some(session) = session {
        command.env("AGIT_SESSION", session);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), requests.len(), "{responses:?}");
    for (response, request) in responses.iter().zip(requests) {
        assert_eq!(response["id"], request["id"]);
        assert_eq!(response["jsonrpc"], "2.0");
    }
    responses
}

/// Protocol initialization cannot migrate saved history before a read-only tool is selected.
#[test]
fn mcp_local_search_preserves_uninitialized_v0_storage_and_login_refusals() {
    for authenticated in [true, false] {
        let lab = Lab::new();
        let repo = lab.seed("demo", meta::LayoutVersion::V0, &[prompt("saved needle")]);
        if !authenticated {
            std::fs::remove_dir_all(lab.temp.path().join("agit/credentials")).unwrap();
        }
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            meta::read_at_ref(&repo, &old).unwrap().layout,
            meta::LayoutVersion::V0
        );
        assert!(!lab.temp.path().join("agit/layout-v1.complete").exists());
        let requests = [
            json!({"jsonrpc":"2.0", "id":1, "method":"initialize"}),
            json!({"jsonrpc":"2.0", "id":2, "method":"tools/list"}),
            json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{"name":"search", "arguments":{"local":true, "query":"needle"}}}),
            json!({"jsonrpc":"2.0", "id":4, "method":"tools/call", "params":{"name":"search", "arguments":{"local":true, "here":true, "query":"needle"}}}),
        ];
        let before = inventory(lab.temp.path());
        for quiet in [false, true] {
            let responses = mcp(&lab, &requests, None, quiet);
            assert_eq!(responses[0]["result"]["serverInfo"]["name"], "agit");
            assert!(
                responses[1]["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "search")
            );
            let result = &responses[2]["result"];
            assert_eq!(result["isError"], !authenticated, "{result}");
            let text = result["content"][0]["text"].as_str().unwrap();
            if authenticated {
                let page: Value = serde_json::from_str(text).unwrap();
                assert_eq!(page["total"], 1, "{page}");
                assert_eq!(page["incomplete"], false, "{page}");
                assert_eq!(page["hits"][0]["agent"], "alice/demo");
                assert_eq!(page["query"], "needle");
            } else {
                assert!(text.starts_with("(exit 5)\n"), "{text}");
            }
            assert_eq!(responses[3]["result"]["isError"], true);
            assert!(
                responses[3]["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("(exit 2)\n")
            );
            assert_eq!(inventory(lab.temp.path()), before);
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old);
            lab.assert_offline();
        }
    }
}

/// The dispatcher must not disable the mutating child command's startup or settlement.
#[test]
fn mcp_commit_migrates_v0_and_settles_the_selected_claim() {
    use agit::domain::{
        link::{self, Link},
        store::Store,
    };
    let lab = Lab::new();
    startup_cache::seed(&lab.temp.path().join("agit"));
    let cwd = lab.temp.path().join("work");
    let native_id = "aaaaaaaa-0000-4000-8000-000000000091";
    let events: Vec<Value> = (1..=2).flat_map(|turn| [
        json!({"type":"user", "sessionId":native_id, "cwd":cwd, "uuid":format!("user-{turn}"), "message":{"role":"user", "content":format!("saved question {turn}")}}),
        json!({"type":"assistant", "sessionId":native_id, "cwd":cwd, "uuid":format!("assistant-{turn}"), "parentUuid":format!("user-{turn}"), "message":{"role":"assistant", "content":format!("saved answer {turn}")}}),
    ]).collect();
    let repo = lab.seed("demo", meta::LayoutVersion::V0, &events[..2]);
    repo.git(&["branch", "-m", "work"]).unwrap();
    let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let original_log = storage::materialize_at(repo.root(), &old, meta::LOG_FILE).unwrap();
    let native_dir = lab
        .temp
        .path()
        .join("home/.claude/projects")
        .join(agit::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&native_dir).unwrap();
    let native_path = native_dir.join(format!("{native_id}.jsonl"));
    let native: String = events.iter().map(|event| format!("{event}\n")).collect();
    std::fs::write(&native_path, &native).unwrap();
    let store = Store::at(lab.temp.path().join("agit/store"));
    let mut claim = Link::new("claude-code", native_id, Some(&cwd));
    claim.owner = Some("alice".into());
    claim.agent = Some("demo".into());
    claim.branch = Some("work".into());
    link::write(&store, &claim).unwrap();
    assert!(!lab.temp.path().join("agit/layout-v1.complete").exists());
    let request = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"commit", "arguments":{}}});
    let responses = mcp(&lab, &[request], Some("alice/demo@work"), false);
    assert_eq!(responses[0]["result"]["isError"], false, "{responses:?}");
    let result: Value = serde_json::from_str(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    let head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    assert_ne!(head, old);
    assert_eq!(
        meta::read_at_ref(&repo, &old).unwrap().layout,
        meta::LayoutVersion::V0
    );
    let metadata = meta::read_at_ref(&repo, &head).unwrap();
    assert_eq!(metadata.layout, meta::LayoutVersion::V1);
    assert_eq!(metadata.turn, Some(2));
    let saved = storage::materialize_at(repo.root(), &head, meta::LOG_FILE).unwrap();
    assert!(saved.starts_with(&original_log));
    assert!(saved.contains("saved question 2") && saved.contains("saved answer 2"));
    let view = storage::materialize_at(repo.root(), &head, meta::VIEW_FILE).unwrap();
    assert!(view.contains("saved question 2") && view.contains("saved answer 2"));
    assert!(
        lab.temp.path().join("agit/layout-v1.complete").is_file(),
        "startup migration did not finish: {result}"
    );
    let current = link::get(&store, "claude-code", native_id).unwrap();
    assert!(link::claims_branch(&current, "alice", "demo", "work"));
    assert_eq!(std::fs::read_to_string(native_path).unwrap(), native);
    assert_eq!(lab.requests.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
struct FifoCarrier {
    path: std::path::PathBuf,
    bytes: Vec<u8>,
    permissions: std::fs::Permissions,
    identity: (u64, u64),
}

#[cfg(unix)]
impl FifoCarrier {
    fn replace(path: std::path::PathBuf) -> Self {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        let bytes = std::fs::read(&path).unwrap();
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        Self {
            path,
            bytes,
            permissions,
            identity: (metadata.dev(), metadata.ino()),
        }
    }

    fn assert_unchanged(&self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let metadata = std::fs::symlink_metadata(&self.path).unwrap();
        assert!(metadata.file_type().is_fifo());
        assert_eq!((metadata.dev(), metadata.ino()), self.identity);
    }
}

#[cfg(unix)]
impl Drop for FifoCarrier {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).unwrap();
        std::fs::write(&self.path, &self.bytes).unwrap();
        std::fs::set_permissions(&self.path, self.permissions.clone()).unwrap();
    }
}

#[cfg(unix)]
fn bounded_fifo_command(command: &mut Command) -> Output {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    let output = tempfile::NamedTempFile::new().unwrap();
    let error = tempfile::NamedTempFile::new().unwrap();
    command
        .stdout(output.reopen().unwrap())
        .stderr(error.reopen().unwrap())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            panic!("local Git inspection did not enforce its operation deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: std::fs::read(output.path()).unwrap(),
        stderr: std::fs::read(error.path()).unwrap(),
    }
}

#[cfg(unix)]
#[test]
fn stalled_git_carriers_keep_verified_hits_and_never_mutate_or_request_the_hub() {
    for stage in ["config", "commit", "legacy-log", "event"] {
        let lab = Lab::new();
        lab.seed(
            "aaa-verified",
            meta::LayoutVersion::V0,
            &[prompt("saved needle")],
        );
        let layout = if stage == "event" {
            meta::LayoutVersion::V1
        } else {
            meta::LayoutVersion::V0
        };
        let repo = lab.seed("zzz-stalled", layout, &[prompt("saved needle")]);
        let path = match stage {
            "config" => repo.root().join(".git/config"),
            _ => {
                let spec = match stage {
                    "commit" => "HEAD".to_owned(),
                    "legacy-log" => format!("HEAD:{}", meta::LEGACY_LOG_FILE),
                    "event" => {
                        let sequence =
                            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap();
                        let ids = storage::parse_sequence(&sequence).unwrap();
                        format!("HEAD:{}", meta::event_path(&ids[0]).unwrap())
                    }
                    _ => unreachable!(),
                };
                let oid = repo.git(&["rev-parse", &spec]).unwrap();
                repo.root()
                    .join(".git/objects")
                    .join(&oid[..2])
                    .join(&oid[2..])
            }
        };
        let before = inventory(lab.temp.path());
        let modes: &[&str] = if matches!(stage, "legacy-log" | "event") {
            &["plain", "quiet", "json1", "json2"]
        } else {
            &["json2"]
        };
        for mode in modes {
            let fifo = FifoCarrier::replace(path.clone());
            let result = value(
                &bounded_fifo_command(lab.command(mode).args(["search", "--local", "needle"])),
                mode,
            );
            assert_eq!(result["total"], 1, "{stage}: {result}");
            assert_eq!(result["hits"][0]["agent"], "alice/aaa-verified");
            assert_eq!(result["incomplete"], true, "{stage}: {result}");
            assert!(!result["incomplete_reasons"].as_array().unwrap().is_empty());
            fifo.assert_unchanged();
            drop(fifo);
            assert_eq!(inventory(lab.temp.path()), before);
            lab.assert_offline();
            let complete = value(
                &lab.command(mode)
                    .args(["search", "--local", "needle"])
                    .output()
                    .unwrap(),
                mode,
            );
            assert_eq!(complete["total"], 2, "{stage}: {complete}");
            assert_eq!(complete["incomplete"], false, "{stage}: {complete}");
            assert_eq!(inventory(lab.temp.path()), before);
            lab.assert_offline();
        }
    }
}

#[test]
fn small_saved_versions_return_hits_after_accounting_for_actual_reads() {
    for layout in [meta::LayoutVersion::V0, meta::LayoutVersion::V1] {
        let lab = Lab::new();
        let repo = lab.seed("demo", layout, &[prompt("needle original")]);
        let sid = format!("agit-{}", "a".repeat(40));
        for version in 1..8 {
            let log = transcript::wrap_lines(
                &format!("{}\n", prompt(&format!("needle version {version}"))),
                "claude-code",
                &sid,
            );
            match layout {
                meta::LayoutVersion::V0 => {
                    std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
                }
                meta::LayoutVersion::V1 => storage::write_snapshot(repo.root(), &log, "").unwrap(),
            }
            lab.commit_at(&repo, version);
        }
        let before = inventory(lab.temp.path());
        for mode in ["plain", "quiet", "json1", "json2"] {
            let result = value(
                &lab.command(mode)
                    .args(["search", "--local", "needle"])
                    .output()
                    .unwrap(),
                mode,
            );
            assert_eq!(result["total"], 8, "{result}");
            assert_eq!(result["incomplete"], false, "{result}");
            assert_eq!(result["hits"].as_array().unwrap().len(), 8);
            assert_eq!(inventory(lab.temp.path()), before);
            lab.assert_offline();
        }
    }
}

#[test]
fn exhausted_scan_budget_keeps_verified_partial_hits() {
    let lab = Lab::new();
    let repo = lab.seed("demo", meta::LayoutVersion::V0, &[prompt("older needle")]);
    for version in 1..=8 {
        std::fs::write(
            repo.root().join(meta::LEGACY_LOG_FILE),
            format!("invalid {version}\n"),
        )
        .unwrap();
        lab.commit_at(&repo, version);
    }
    let sid = format!("agit-{}", "a".repeat(40));
    let log = transcript::wrap_lines(
        &format!("{}\n", prompt("latest needle")),
        "claude-code",
        &sid,
    );
    std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
    lab.commit_at(&repo, 30);
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let before = inventory(lab.temp.path());
    for mode in ["plain", "quiet", "json1", "json2"] {
        let result = value(
            &lab.command(mode)
                .args(["search", "--local", "needle"])
                .output()
                .unwrap(),
            mode,
        );
        assert_eq!(result["total"], 1, "{result}");
        assert_eq!(result["hits"][0]["commit"], head);
        assert_eq!(result["incomplete"], true);
        let reasons = result["incomplete_reasons"].as_array().unwrap();
        assert!(reasons.contains(&json!("read_budget")), "{result}");
        assert!(!reasons.contains(&json!("unverified_snapshot")), "{result}");
        assert_eq!(inventory(lab.temp.path()), before);
        lab.assert_offline();
    }
}

#[test]
fn saved_v0_v1_history_is_searchable_without_view_native_or_network_access() {
    for mode in ["plain", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        for (name, layout) in [
            ("legacy", meta::LayoutVersion::V0),
            ("modern", meta::LayoutVersion::V1),
        ] {
            lab.seed(
                name,
                layout,
                &[prompt("saved needle"), prompt("saved needle")],
            );
        }
        let before = inventory(lab.temp.path());
        let output = lab
            .command(mode)
            .args([
                "search", "--local", "saved", "--in", "prompt", "--sort", "recent", "--limit", "1",
            ])
            .output()
            .unwrap();
        let result = value(&output, mode);
        assert_eq!(result["corpus"], "local_saved_history");
        assert_eq!(result["total"], 2);
        assert_eq!(result["has_more"], true);
        assert_eq!(result["incomplete"], false);
        assert_eq!(
            result["hits"][0]["turns"], 2,
            "turn count comes from the saved event groups, not the commit ordinal"
        );
        assert_eq!(
            result["hits"][0]["other_hits"], 1,
            "repeated occurrences remain counted"
        );
        let second = value(
            &lab.command(mode)
                .args([
                    "search", "--local", "saved", "--in", "prompt", "--sort", "recent", "--limit",
                    "1", "--page", "2",
                ])
                .output()
                .unwrap(),
            mode,
        );
        assert_ne!(result["hits"][0]["agent"], second["hits"][0]["agent"]);
        let missing = value(
            &lab.command(mode)
                .args(["search", "--local", "native-only-needle"])
                .output()
                .unwrap(),
            mode,
        );
        assert_eq!(missing["total"], 0);
        assert_eq!(missing["incomplete"], false);
        assert_eq!(before, inventory(lab.temp.path()));
        lab.assert_offline();
    }
}

#[test]
fn offline_batch_filters_run_before_deduplication_and_retain_input_order() {
    let lab = Lab::new();
    let tool = json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"call-a", "name":"Bash", "input":{"command":"echo argument-only-needle"}}]}});
    lab.seed(
        "demo",
        meta::LayoutVersion::V1,
        &[prompt("user needle"), tool],
    );
    let before = inventory(lab.temp.path());
    let output = lab
        .command("json2")
        .args([
            "search",
            "--local",
            "--query",
            "argument-only-needle in:tool tool:Bash",
            "--query",
            "needle in:prompt",
            "--repo",
            "alice/demo",
        ])
        .output()
        .unwrap();
    let result = value(&output, "json2");
    assert_eq!(result["batch"], true);
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert_eq!(result["results"][0]["result"]["total"], 1);
    assert_eq!(result["results"][0]["result"]["hits"][0]["scope"], "tool");
    assert!(
        result["results"][0]["result"]["hits"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("argument-only-needle")
    );
    assert_eq!(result["results"][1]["result"]["hits"][0]["scope"], "prompt");
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn unsupported_local_requests_fail_before_login_or_corpus_inspection() {
    let lab = Lab::new();
    std::fs::remove_dir_all(lab.temp.path().join("agit/credentials")).unwrap();
    std::fs::write(lab.temp.path().join("agit/repos"), "not a directory").unwrap();
    let before = inventory(lab.temp.path());
    for mode in ["plain", "quiet", "json1", "json2"] {
        for args in [
            vec!["search", "--local", "needle", "--type", "people"],
            vec!["search", "--local", "needle", "--counts"],
            vec!["search", "--local", "needle", "--author", "alice"],
            vec!["search", "--local", "is:public"],
            vec!["search", "--local", "category:research"],
            vec!["search", "--local", "state:open"],
            vec!["search", "--local", "runtim:codex"],
            vec!["search", "--local", "\"\""],
            vec!["search", "--local", "--scope", "mine", "needle"],
            vec!["search", "--local", "--here", "needle"],
        ] {
            let output = lab.command(mode).args(args).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(2),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn remote_scope_combinations_refuse_before_git_login_or_local_inspection() {
    for logged_in in [true, false] {
        let lab = Lab::new();
        lab.seed("demo", meta::LayoutVersion::V1, &[prompt("saved needle")]);
        if !logged_in {
            std::fs::remove_dir_all(lab.temp.path().join("agit/credentials")).unwrap();
        }
        let trace = lab.temp.path().join("unexpected-git-trace");
        let before = inventory(lab.temp.path());
        for mode in ["plain", "quiet", "json1", "json2"] {
            for options in [
                vec!["--scope", "mine"],
                vec!["--scope", "org"],
                vec!["--scope", "public"],
                vec!["--scope", "alice/demo"],
                vec!["--here"],
                vec!["--here", "--scope", "org"],
                vec!["--scope", "org", "--query", "other needle"],
            ] {
                let output = lab
                    .command(mode)
                    .env("GIT_TRACE", &trace)
                    .args(["search", "--local", "needle"])
                    .args(&options)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(2), "{mode}: {options:?}");
                let diagnostic = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    diagnostic.contains("--local cannot be combined with --scope or --here"),
                    "{mode}: {options:?}: {diagnostic}"
                );
                assert!(!trace.exists(), "unsupported local scopes cannot start Git");
                lab.assert_offline();
            }
            let output = lab
                .command(mode)
                .args(["search", "--local", "needle", "--repo", "alice/demo"])
                .output()
                .unwrap();
            if logged_in {
                let result = value(&output, mode);
                assert_eq!(result["total"], 1);
                assert_eq!(result["incomplete"], false);
                assert_eq!(result["hits"][0]["agent"], "alice/demo");
            } else {
                assert_eq!(output.status.code(), Some(5));
            }
        }
        assert_eq!(before, inventory(lab.temp.path()));
        lab.assert_offline();
    }
}

#[test]
fn missing_local_login_and_absent_corpus_never_create_state_or_refresh() {
    let lab = Lab::new();
    let before = inventory(lab.temp.path());
    let result = value(
        &lab.command("json1")
            .args(["search", "--local", "needle"])
            .output()
            .unwrap(),
        "json1",
    );
    assert_eq!(result["total"], 0);
    assert_eq!(result["incomplete"], false);
    assert_eq!(before, inventory(lab.temp.path()));
    std::fs::remove_dir_all(lab.temp.path().join("agit/credentials")).unwrap();
    let before = inventory(lab.temp.path());
    let output = lab
        .command("json2")
        .args(["search", "--local", "needle"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn corrupt_or_oversized_saved_history_is_explicitly_incomplete() {
    let lab = Lab::new();
    let repo = lab.seed("demo", meta::LayoutVersion::V1, &[prompt("needle")]);
    std::fs::write(
        repo.root().join(meta::LOG_FILE),
        "invalid event reference\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("corrupt saved log").unwrap();
    lab.seed(
        "large",
        meta::LayoutVersion::V1,
        &[prompt(&"x".repeat(2 * 1024 * 1024 + 1))],
    );
    let before = inventory(lab.temp.path());
    let result = value(
        &lab.command("json2")
            .args(["search", "--local", "needle"])
            .output()
            .unwrap(),
        "json2",
    );
    assert_eq!(result["incomplete"], true);
    assert!(
        result["total"].as_u64().unwrap() >= 1,
        "readable parent evidence remains searchable"
    );
    assert!(
        result["incomplete_reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("unavailable_saved_log"))
    );
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn raw_saved_parent_history_survives_local_shallow_and_graft_views() {
    let lab = Lab::new();
    let repo = lab.seed(
        "demo",
        meta::LayoutVersion::V1,
        &[prompt("parent-only-needle")],
    );
    let sid = format!("agit-{}", "a".repeat(40));
    let raw = format!("{}\n", prompt("different tip"));
    let log = transcript::wrap_lines(&raw, "claude-code", &sid);
    storage::write_snapshot(repo.root(), &log, "").unwrap();
    repo.add_all().unwrap();
    repo.commit("different saved tip").unwrap();
    let tip = repo.git(&["rev-parse", "HEAD"]).unwrap();
    std::fs::write(repo.root().join(".git/shallow"), format!("{tip}\n")).unwrap();
    std::fs::create_dir_all(repo.root().join(".git/info")).unwrap();
    std::fs::write(repo.root().join(".git/info/grafts"), format!("{tip}\n")).unwrap();
    let before = inventory(lab.temp.path());
    let result = value(
        &lab.command("plain")
            .args(["search", "--local", "parent-only-needle"])
            .output()
            .unwrap(),
        "plain",
    );
    assert_eq!(result["total"], 1);
    assert_ne!(result["hits"][0]["commit"], tip);
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[cfg(unix)]
#[test]
fn symlinked_local_corpus_entries_are_not_followed() {
    use std::os::unix::fs::symlink;
    let lab = Lab::new();
    let repo = lab.seed("demo", meta::LayoutVersion::V1, &[prompt("needle")]);
    symlink(repo.root(), repo.root().parent().unwrap().join("alias")).unwrap();
    let before = inventory(lab.temp.path());
    let result = value(
        &lab.command("plain")
            .args(["search", "--local", "needle"])
            .output()
            .unwrap(),
        "plain",
    );
    assert_eq!(result["total"], 1);
    assert_eq!(result["incomplete"], true);
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn a_carrierless_repo_name_cannot_relabel_parent_history() {
    let lab = Lab::new();
    let outer = lab.seed(
        "outer",
        meta::LayoutVersion::V0,
        &[prompt("parent-only-needle")],
    );
    let nested_home = outer.root().join("nested-agit");
    std::fs::create_dir_all(nested_home.join("repos/alice/phantom")).unwrap();
    std::fs::create_dir_all(nested_home.join("credentials")).unwrap();
    let credentials = std::fs::read_dir(lab.temp.path().join("agit/credentials"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    std::fs::copy(
        credentials.path(),
        nested_home
            .join("credentials")
            .join(credentials.file_name()),
    )
    .unwrap();
    let before = inventory(lab.temp.path());
    for mode in ["plain", "quiet", "json1", "json2"] {
        let output = lab
            .command(mode)
            .env("AGIT_HOME", &nested_home)
            .args([
                "search",
                "--local",
                "parent-only-needle",
                "--repo",
                "alice/phantom",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let result = value(&output, mode);
        assert_eq!(result["total"], 0);
        assert_eq!(result["incomplete"], true);
        assert!(
            result["incomplete_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "unavailable_repository")
        );
    }
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

#[test]
fn dense_saved_blocks_exhaust_the_command_work_budget_without_native_or_file_writes() {
    let lab = Lab::new();
    let content = json!({"type":"assistant", "message":{"role":"assistant", "content":vec![json!({"type":"text", "text":"needle"}); 30_000]}});
    let repo = lab.seed("dense", meta::LayoutVersion::V0, &[content]);
    let before = inventory(lab.temp.path());
    for mode in ["plain", "quiet", "json1", "json2"] {
        for query in ["needle", "needle turns:>999"] {
            let output = lab
                .command(mode)
                .args(["search", "--local", query])
                .output()
                .unwrap();
            let result = value(&output, mode);
            assert_eq!(result["total"], 0);
            assert_eq!(result["incomplete"], true);
        }
    }
    assert!(
        repo.root()
            .join("session/log.jsonl")
            .metadata()
            .unwrap()
            .len()
            < 2 * 1024 * 1024
    );
    assert_eq!(before, inventory(lab.temp.path()));
    lab.assert_offline();
}

fn codex_identity() -> Value {
    json!({"type":"session_meta", "timestamp":"2026-01-01T00:00:00Z",
        "payload":{"id":"synthetic-codex", "cwd":"/synthetic", "timestamp":"2026-01-01T00:00:00Z"}})
}

fn codex_prompt() -> Value {
    json!({"type":"response_item", "payload":{"type":"message", "role":"user",
        "content":[{"type":"input_text", "text":"saved turn needle"}]}})
}

#[test]
fn codex_identity_metadata_preserves_exact_saved_turn_filters() {
    for layout in [meta::LayoutVersion::V0, meta::LayoutVersion::V1] {
        let lab = Lab::new();
        lab.seed_runtime(
            "codex",
            layout,
            "codex",
            &[codex_identity(), codex_prompt()],
        );
        let before = inventory(lab.temp.path());
        for mode in ["human", "quiet", "json1", "json2"] {
            for query in ["needle", "needle turns:1", "needle turns:2"] {
                let result = value(
                    &lab.command(mode)
                        .args(["search", "--local", query])
                        .output()
                        .unwrap(),
                    mode,
                );
                assert_eq!(result["incomplete"], false, "{result}");
                let expected = if query.ends_with("turns:2") { 0 } else { 1 };
                assert_eq!(result["total"], expected, "{result}");
                if expected == 1 {
                    let hit = &result["hits"][0];
                    assert_eq!(hit["turns"], 1);
                    assert_eq!(hit["turns_incomplete"], false);
                    assert_eq!(hit["scope"], "prompt");
                    assert_eq!(hit["line"], 2);
                    assert!(
                        hit["excerpt"]
                            .as_str()
                            .unwrap()
                            .contains("saved turn needle")
                    );
                }
                assert_eq!(inventory(lab.temp.path()), before);
                lab.assert_offline();
            }
        }
    }
}

#[test]
fn undisplayed_non_user_content_and_unknown_records_have_distinct_turn_reliability() {
    let cases = [
        (
            json!({"type":"response_item", "payload":{"type":"reasoning", "encrypted_content":"synthetic-unreadable", "summary":[]}}),
            false,
        ),
        (
            json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"synthetic-call", "output":{"opaque":[true]}}}),
            false,
        ),
        (
            json!({"type":"turn_context", "payload":{"cwd":"/synthetic", "model":"synthetic"}}),
            false,
        ),
        (
            json!({"type":"event_msg", "payload":{"type":"patch_apply_end", "changes":{"/synthetic/file.rs":{}}, "success":true}}),
            false,
        ),
        (
            json!({"type":"future-record", "payload":{"text":"could be a user turn"}}),
            true,
        ),
        (
            json!({"type":"response_item", "payload":{"type":"future-response", "text":"could be a user turn"}}),
            true,
        ),
        (
            json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"future-user-block", "value":"unreadable"}]}}),
            true,
        ),
    ];
    for (record, turns_incomplete) in cases {
        let lab = Lab::new();
        lab.seed_runtime(
            "codex",
            meta::LayoutVersion::V1,
            "codex",
            &[codex_identity(), codex_prompt(), record],
        );
        let before = inventory(lab.temp.path());
        for mode in ["human", "quiet", "json1", "json2"] {
            let result = value(
                &lab.command(mode)
                    .args(["search", "--local", "needle"])
                    .output()
                    .unwrap(),
                mode,
            );
            assert_eq!(result["total"], 1, "{result}");
            assert_eq!(result["incomplete"], true, "{result}");
            assert_eq!(result["hits"][0]["turns"], 1);
            assert_eq!(
                result["hits"][0]["turns_incomplete"], turns_incomplete,
                "{result}"
            );
            let filtered = value(
                &lab.command(mode)
                    .args(["search", "--local", "needle turns:1"])
                    .output()
                    .unwrap(),
                mode,
            );
            assert_eq!(
                filtered["total"],
                if turns_incomplete { 0 } else { 1 },
                "{filtered}"
            );
            assert_eq!(filtered["incomplete"], true, "{filtered}");
            assert_eq!(inventory(lab.temp.path()), before);
            lab.assert_offline();
        }
    }
}
