//! Push and RC landing preserve known failure categories and never invent replacement history.

#[path = "support/publication_http.rs"]
mod publication_http;
#[path = "support/publication_process.rs"]
mod publication_process;

use agit::domain::{meta, repo::Repo};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const AGENT_ID: &str = "9f2c3b53-7fe0-412f-b62a-bf68a6845ce7";
const OTHER_ID: &str = "070fd8ab-ef31-4f1e-843f-728e2496f6eb";

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    hub: TcpListener,
    base: String,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        for name in ["work", "tmp", "templates"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let base = format!("http://{}", hub.local_addr().unwrap());
        let lab = Self {
            root,
            home,
            hub,
            base,
        };
        let output = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["config", "hub.url"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let credential = agit::infra::credentials::HubCredential {
            username: "alice".into(),
            email: None,
            hub: Some(lab.base.clone()),
            access_token: "synthetic-publication-token".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-publication-refresh".into(),
            refresh_expires_at: "2000-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &lab.home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&lab.base).unwrap()
            )),
            &credential,
        )
        .unwrap();
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for name in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.base)
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Publication category fixture")
            .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
            .env("GIT_COMMITTER_NAME", "Publication category fixture")
            .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(unix) { "file" } else { "os" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(self.root.path().join("work"))
            .stdin(Stdio::null());
        command
    }

    fn git(&self, path: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn seed(&self, owner: &str, name: &str, metadata: bool) -> PathBuf {
        let path = self.home.join("repos").join(owner).join(name);
        fs::create_dir_all(&path).unwrap();
        self.git(&path, &["init", "--initial-branch=main"]);
        self.git(&path, &["config", "commit.gpgsign", "false"]);
        if metadata {
            meta::write(&path, &meta::Meta::new_file_line()).unwrap();
        } else {
            fs::write(path.join("AGENTS.md"), "Synthetic shared content.\n").unwrap();
        }
        self.git(&path, &["add", "."]);
        self.git(
            &path,
            &["commit", "-m", "Create synthetic publication history"],
        );
        path
    }

    fn push(&self, target: &str, mode: &str, dry_run: bool) -> Output {
        self.push_with_visibility(target, mode, dry_run, Some("--private"))
    }

    fn push_with_visibility(
        &self,
        target: &str,
        mode: &str,
        dry_run: bool,
        visibility: Option<&str>,
    ) -> Output {
        self.push_command(target, mode, dry_run, visibility)
            .output()
            .unwrap()
    }

    fn push_command(
        &self,
        target: &str,
        mode: &str,
        dry_run: bool,
        visibility: Option<&str>,
    ) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        command.args(["push", target, "-b", "main"]);
        if let Some(flag) = visibility {
            command.arg(flag);
        }
        if dry_run {
            command.arg("--dry-run");
        }
        command
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                let data = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    data,
                )
            })
            .collect()
    }

    fn no_requests(&self) {
        assert_eq!(
            self.hub.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        if let Ok(bytes) = fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

#[derive(Clone, Debug)]
enum Reply {
    Status(u16),
    Json(Value),
    Truncated,
}

struct Step {
    request: String,
    reply: Reply,
    body: Option<Value>,
}

impl Step {
    fn new(request: &str, reply: Reply) -> Self {
        Self {
            request: request.into(),
            reply,
            body: None,
        }
    }

    fn with_body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
}

struct Server {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Vec<String>>>,
}

impl Server {
    fn start(lab: &Lab, steps: Vec<Step>) -> Self {
        let listener = lab.hub.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut requests = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "loopback deadline elapsed");
                let (mut stream, _) = match publication_http::accept(&listener) {
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
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    header.push(byte[0]);
                    assert!(header.len() <= 65536, "request header exceeded its bound");
                }
                let header = String::from_utf8(header).unwrap();
                let first = header.lines().next().unwrap();
                let step = steps
                    .get(requests.len())
                    .expect("unexpected request replay or creation");
                assert_eq!(first, format!("{} HTTP/1.1", step.request));
                let field = |key: &str| {
                    header.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case(key).then_some(value.trim())
                    })
                };
                assert_eq!(
                    field("Authorization"),
                    Some("Bearer synthetic-publication-token")
                );
                if first.contains("service=git-receive-pack")
                    || first.contains("service=git-upload-pack")
                {
                    assert_eq!(field(identity::EXPECTED_AGENT_ID_HEADER), Some(AGENT_ID));
                }
                let length =
                    field("Content-Length").map_or(0, |value| value.parse::<usize>().unwrap());
                assert!(length <= 65536, "request body exceeded its bound");
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if let Some(expected) = &step.body {
                    assert_eq!(&serde_json::from_slice::<Value>(&body).unwrap(), expected);
                } else if first == "POST /api/agents HTTP/1.1" {
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["name"], "qa");
                    assert_eq!(value["public"], false);
                    assert!(value.get("owner").is_none_or(Value::is_null));
                    assert!(
                        !String::from_utf8_lossy(&body).contains("synthetic-publication-token")
                    );
                }
                requests.push(step.request.clone());
                let (status, body) = match &step.reply {
                    Reply::Status(status) => (
                        *status,
                        json!({
                            "error":"HTTP 401: agit login (synthetic spoof body)",
                            "kind":"unauthorized"
                        })
                        .to_string(),
                    ),
                    Reply::Json(value) => (200, value.to_string()),
                    Reply::Truncated => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Len").unwrap();
                        stream.flush().unwrap();
                        continue;
                    }
                };
                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
            }
            assert_eq!(
                requests.len(),
                steps.len(),
                "the expected request boundary was not reached"
            );
            requests
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        publication_http::join(self.worker.take().unwrap()).expect("loopback worker failed")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let joined = publication_http::join(worker);
            if !thread::panicking() {
                assert!(
                    joined.is_ok(),
                    "loopback worker failed during cleanup: {joined:?}"
                );
            }
        }
    }
}

fn remote(lab: &Lab, owner: &str, id: &str) -> Value {
    json!({"agent_id":id, "owner":owner, "name":"qa", "visibility":"private",
        "clone_url":format!("{}/{owner}/qa.git", lab.base)})
}

fn assert_failure(output: &Output, mode: &str, code: i32) -> String {
    assert_command_failure(output, mode, code, "push")
}

fn assert_command_failure(output: &Output, mode: &str, code: i32, command: &str) -> String {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    let text = if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
        assert_eq!(value["command"], command);
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], false);
        value["diagnostics"]["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::from_utf8(output.stderr.clone()).unwrap()
    };
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for secret in [
            "synthetic-publication-token",
            "synthetic-publication-refresh",
        ] {
            assert!(!text.contains(secret), "credential reached command output");
        }
    }
    text
}

#[test]
fn push_http_boundaries_preserve_auth_without_creating_fallback_repositories() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for stage in [
            "owned-get",
            "pinned-get",
            "legacy-get",
            "create-post",
            "foreign-get",
            "foreign-access",
            "foreign-org",
        ] {
            for (reply, code) in [
                (Reply::Status(401), 5),
                (Reply::Status(503), 6),
                (Reply::Truncated, 6),
            ] {
                let lab = Lab::new();
                let owner = if stage.starts_with("foreign-") {
                    "other"
                } else {
                    "alice"
                };
                let target = format!("{owner}/qa");
                let path = lab.seed(owner, "qa", true);
                if stage == "pinned-get" {
                    identity::pin(
                        &Repo::at(&path),
                        &RemoteIdentity::new(&lab.base, AGENT_ID).unwrap(),
                    )
                    .unwrap();
                }
                if stage == "legacy-get" {
                    lab.git(
                        &path,
                        &[
                            "remote",
                            "add",
                            "origin",
                            &format!("{}/alice/qa.git", lab.base),
                        ],
                    );
                }
                // Warm only the ordinary local scan state; no Hub call or publication is admitted.
                let dry = lab.push(&target, "human", true);
                assert!(dry.status.success(), "{stage}: {dry:?}");
                lab.no_requests();
                let before = lab.state();
                let refs = lab.git(&path, &["show-ref"]);
                let get = format!("GET /api/agents/{target}");
                let steps = match stage {
                    "create-post" => vec![
                        Step::new(&get, Reply::Status(404)),
                        Step::new("POST /api/agents", reply),
                    ],
                    "foreign-access" => vec![
                        Step::new(&get, Reply::Json(remote(&lab, owner, AGENT_ID))),
                        Step::new(
                            "GET /other/qa.git/info/refs?service=git-receive-pack",
                            reply,
                        ),
                    ],
                    "foreign-org" => vec![
                        Step::new(&get, Reply::Status(404)),
                        Step::new("GET /api/orgs/other", reply),
                    ],
                    _ => vec![Step::new(&get, reply)],
                };
                let server = Server::start(&lab, steps);
                let output = lab.push(&target, mode, false);
                server.finish();
                assert_failure(&output, mode, code);
                assert_eq!(lab.state(), before, "{stage}: {mode} changed local files");
                assert_eq!(lab.git(&path, &["show-ref"]), refs);
                if owner == "other" {
                    assert!(!lab.home.join("repos/alice/qa").exists());
                }
                lab.no_requests();
            }
        }
    }
}

#[test]
fn organization_creation_preserves_private_defaults_and_never_retries_as_public() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for (visibility, public) in [
            (None, false),
            (Some("--private"), false),
            (Some("--public"), true),
        ] {
            for rejection in [428, 503] {
                let lab = Lab::new();
                let path = lab.seed("team", "qa", true);
                let dry = lab.push("team/qa", "human", true);
                assert!(dry.status.success(), "{dry:?}");
                lab.no_requests();
                let before = lab.state();
                let refs = lab.git(&path, &["show-ref"]);
                let server = Server::start(
                    &lab,
                    vec![
                        Step::new("GET /api/agents/team/qa", Reply::Status(404)),
                        Step::new(
                            "GET /api/orgs/team",
                            Reply::Json(json!({"name":"team", "role":"owner"})),
                        ),
                        Step::new("GET /api/agents/team/qa", Reply::Status(404)),
                        Step::new("POST /api/agents", Reply::Status(rejection)).with_body(json!({
                            "name":"qa", "owner":"team", "public":public, "repo_origins":[]
                        })),
                    ],
                );
                let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
                if mode == "quiet" {
                    command.arg("--quiet");
                }
                if let Some(version) = mode.strip_prefix("json") {
                    command.args(["--json", "--json-version", version]);
                }
                command.args(["push", "team/qa", "-b", "main"]);
                if let Some(flag) = visibility {
                    command.arg(flag);
                }
                let output = command.output().unwrap();
                server.finish();
                assert_failure(&output, mode, 6);
                assert_eq!(lab.state(), before, "{mode}/{visibility:?}/{rejection}");
                assert_eq!(lab.git(&path, &["show-ref"]), refs);
                assert!(!lab.home.join("repos/alice/qa").exists());
                lab.no_requests();
            }
        }
    }
}

#[test]
fn first_publication_confirms_current_identity_and_visibility_before_pinning_or_uploading() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for case in [
            "default-public-race",
            "explicit-public-race",
            "changed-id",
            "changed-owner",
            "changed-name",
            "post-owner",
            "confirmation-auth",
            "confirmation-unavailable",
            "confirmed-private",
        ] {
            let lab = Lab::new();
            let path = lab.seed("team", "qa", true);
            let dry = lab.push("team/qa", "human", true);
            assert!(dry.status.success(), "{dry:?}");
            lab.no_requests();
            let before = lab.state();
            let refs = lab.git(&path, &["show-ref"]);
            let confirmed = case == "confirmed-private";
            let expected = if confirmed {
                let config = path.join(".git/config");
                let original = fs::read(&config).unwrap();
                identity::pin(
                    &Repo::at(&path),
                    &RemoteIdentity::new(&lab.base, AGENT_ID).unwrap(),
                )
                .unwrap();
                lab.git(
                    &path,
                    &[
                        "remote",
                        "add",
                        "origin",
                        &format!("{}/team/qa.git", lab.base),
                    ],
                );
                let expected = lab.state();
                fs::write(config, original).unwrap();
                assert_eq!(lab.state(), before);
                expected
            } else {
                before
            };
            let mut response = remote(&lab, "team", AGENT_ID);
            match case {
                "default-public-race" | "explicit-public-race" => {
                    response["visibility"] = json!("public");
                }
                "changed-id" => response["agent_id"] = json!(OTHER_ID),
                "changed-owner" => response["owner"] = json!("other"),
                "changed-name" => response["name"] = json!("other"),
                _ => {}
            }
            let (confirmation, code) = match case {
                "confirmation-auth" => (Reply::Status(401), 5),
                "confirmation-unavailable" => (Reply::Status(503), 6),
                "confirmed-private" => (Reply::Json(response), 6),
                _ => (Reply::Json(response), 4),
            };
            let mut steps = vec![
                Step::new("GET /api/agents/team/qa", Reply::Status(404)),
                Step::new(
                    "GET /api/orgs/team",
                    Reply::Json(json!({"name":"team", "role":"owner"})),
                ),
                Step::new("GET /api/agents/team/qa", Reply::Status(404)),
                Step::new(
                    "POST /api/agents",
                    Reply::Json(json!({
                        "agent_id":AGENT_ID,
                        "owner":if case == "post-owner" { "other" } else { "team" },
                        "name":"qa",
                        "push_url":format!("{}/team/qa.git", lab.base),
                        "web_url":format!("{}/team/qa", lab.base)
                    })),
                )
                .with_body(json!({
                    "name":"qa", "owner":"team", "public":false, "repo_origins":[]
                })),
                Step::new("GET /api/agents/team/qa", confirmation),
            ];
            if confirmed {
                steps.push(Step::new(
                    "GET /team/qa.git/info/refs?service=git-receive-pack",
                    Reply::Status(503),
                ));
            }
            let server = Server::start(&lab, steps);
            let visibility = (case != "default-public-race").then_some("--private");
            let output = lab.push_with_visibility("team/qa", mode, false, visibility);
            server.finish();
            let message = assert_failure(&output, mode, code);
            if code == 4 {
                assert!(message.contains("nothing was uploaded"), "{message}");
            }
            assert_eq!(
                lab.state(),
                expected,
                "{case}/{mode} changed local evidence"
            );
            assert_eq!(lab.git(&path, &["show-ref"]), refs);
            assert!(!lab.home.join("repos/alice/qa").exists());
            lab.no_requests();
        }
    }
}

#[test]
fn identity_constraints_and_invalid_remote_ids_refuse_without_writes() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for case in [
            "wrong-hub",
            "reused-name",
            "invalid-id",
            "missing-pin",
            "pinned-404",
        ] {
            let lab = Lab::new();
            let path = lab.seed("alice", "qa", true);
            if case != "missing-pin" {
                let hub = if case == "wrong-hub" {
                    "http://wrong-hub.invalid"
                } else {
                    &lab.base
                };
                identity::pin(
                    &Repo::at(&path),
                    &RemoteIdentity::new(hub, AGENT_ID).unwrap(),
                )
                .unwrap();
            }
            let dry = lab.push("alice/qa", "human", true);
            assert!(dry.status.success(), "{dry:?}");
            lab.no_requests();
            let before = lab.state();
            let reply = match case {
                "pinned-404" => Reply::Status(404),
                "invalid-id" => Reply::Json(remote(&lab, "alice", "invalid-id")),
                _ => Reply::Json(remote(&lab, "alice", OTHER_ID)),
            };
            let steps = if matches!(case, "wrong-hub" | "missing-pin") {
                Vec::new()
            } else {
                vec![Step::new("GET /api/agents/alice/qa", reply)]
            };
            let server = Server::start(&lab, steps);
            let mut command = lab.push_command("alice/qa", mode, false, Some("--private"));
            if case != "invalid-id" {
                command.env("AGIT_EXPECTED_AGENT_ID", AGENT_ID);
            }
            let output = command.output().unwrap();
            server.finish();
            let code = if case == "pinned-404" { 6 } else { 1 };
            let message = assert_failure(&output, mode, code);
            assert!(!message.contains("publishing qa"));
            assert_eq!(lab.state(), before, "{case}: {mode} changed local files");
            lab.no_requests();
        }
    }
}

#[test]
fn branch_git_failures_preserve_known_categories_without_pushing_tags_or_new_repositories() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for (status, code) in [
            (401, 5),
            (403, 7),
            (409, 7),
            (412, 4),
            (413, 7),
            (422, 7),
            (428, 4),
            (503, 6),
        ] {
            let lab = Lab::new();
            let path = lab.seed("alice", "qa", true);
            identity::pin(
                &Repo::at(&path),
                &RemoteIdentity::new(&lab.base, AGENT_ID).unwrap(),
            )
            .unwrap();
            let dry = lab.push("alice/qa", "human", true);
            assert!(dry.status.success(), "{dry:?}");
            lab.no_requests();
            let before = lab.state();
            let refs = lab.git(&path, &["show-ref"]);
            let url = format!("{}/alice/qa.git", lab.base);
            // A successful identity lookup records origin before the transport runs; only that
            // known config change belongs in the failed publication's expected inventory.
            lab.git(&path, &["remote", "add", "origin", &url]);
            let expected = lab.state();
            lab.git(&path, &["remote", "remove", "origin"]);
            assert_eq!(lab.state(), before);
            let server = Server::start(
                &lab,
                vec![
                    Step::new(
                        "GET /api/agents/alice/qa",
                        Reply::Json(remote(&lab, "alice", AGENT_ID)),
                    ),
                    Step::new(
                        "GET /alice/qa.git/info/refs?service=git-receive-pack",
                        Reply::Status(status),
                    ),
                ],
            );
            let output = lab.push("alice/qa", mode, false);
            server.finish();
            assert_failure(&output, mode, code);
            assert_eq!(lab.git(&path, &["show-ref"]), refs);
            assert_eq!(
                lab.state(),
                expected,
                "HTTP {status}: {mode} changed publication state"
            );
            assert!(!lab.home.join("repos/alice/qa-2").exists());
            lab.no_requests();
        }
    }
}

fn rc_land(lab: &Lab, mode: &str, stage: &str) -> Output {
    let mut command = lab.command(env!("CARGO_BIN_EXE_agit"));
    if mode == "quiet" {
        command.arg("--quiet");
    }
    if let Some(version) = mode.strip_prefix("json") {
        command.args(["--json", "--json-version", version]);
    }
    command.args(agit::commands::rc::land_argv(
        "alice/qa",
        AGENT_ID,
        "rc-http",
        "codex",
        "synthetic-rc-land",
        lab.root.path().join("work").to_str().unwrap(),
    ));
    publication_process::output(
        command,
        mode,
        stage,
        Instant::now() + publication_process::MODE_LIMIT,
    )
    .unwrap()
}

fn seed_rc_native_canary(lab: &Lab) {
    let native = lab.root.path().join(".codex/sessions/native-canary.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(native, b"retained native transcript evidence\n").unwrap();
}

fn assert_rc_failure(output: &Output, mode: &str, code: i32) -> String {
    let text = assert_command_failure(output, mode, code, "rc");
    assert!(!text.contains("retained native transcript evidence"));
    if let Some(version) = mode.strip_prefix("json") {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["result"]["format"], "empty");
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else {
            assert_eq!(value["fix"], json!([]));
        }
    } else {
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    text
}

#[test]
fn rc_land_local_recovery_and_reused_identity_fail_before_history_mutation() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for case in ["migration-lock", "reused-name"] {
            let lab = Lab::new();
            seed_rc_native_canary(&lab);
            if case == "migration-lock" {
                let lock = lab.home.join("layout-v1.lock");
                assert!(!lock.exists());
                fs::create_dir(lock).unwrap();
            }
            let before = lab.state();
            let server = Server::start(
                &lab,
                vec![Step::new(
                    "GET /api/agents/alice/qa",
                    Reply::Json(remote(
                        &lab,
                        "alice",
                        if case == "reused-name" {
                            OTHER_ID
                        } else {
                            AGENT_ID
                        },
                    )),
                )],
            );
            let output = rc_land(&lab, mode, case);
            assert_eq!(server.finish(), vec!["GET /api/agents/alice/qa"]);
            let text = assert_rc_failure(&output, mode, 4);
            if case == "migration-lock" {
                assert!(
                    text.contains("cannot prepare RC history recovery"),
                    "{text}"
                );
                assert!(text.contains("cannot open migration lock"), "{text}");
            } else {
                assert!(text.contains("refusing a reused name"), "{text}");
                assert!(text.contains(AGENT_ID) && text.contains(OTHER_ID), "{text}");
            }
            assert_eq!(lab.state(), before, "{mode}/{case}");
            assert!(!lab.home.join("repos").exists());
            assert!(!lab.home.join("store").exists());
            assert!(!lab.home.join("rc/agitd.pid").exists());
            lab.no_requests();
        }
    }
}

fn assert_rc_clone_recovery(
    lab: &Lab,
    before: BTreeMap<PathBuf, Option<Vec<u8>>>,
    clone_started: bool,
) {
    let directory = lab.home.join("layout-v1-recovery");
    let pending = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("pending-rc-land-history-")
    );
    let evidence = format!("{}\n", Path::new("alice").join("qa").display()).into_bytes();
    assert_eq!(fs::read(&pending[0]).unwrap(), evidence);
    let mut expected = before;
    let relative = |path: &Path| path.strip_prefix(lab.root.path()).unwrap().to_owned();
    // A failed clone retains its recovery witness; only successful history completion clears it.
    assert!(expected.insert(relative(&directory), None).is_none());
    assert!(
        expected
            .insert(relative(&pending[0]), Some(evidence))
            .is_none()
    );
    assert!(
        expected
            .insert(relative(&lab.home.join("layout-v1.lock")), Some(Vec::new()))
            .is_none()
    );
    if clone_started {
        for path in [lab.home.join("repos"), lab.home.join("repos/alice")] {
            assert!(expected.insert(relative(&path), None).is_none());
        }
        let destination = lab.home.join("repos/alice/qa");
        if destination.exists() {
            assert!(destination.is_dir());
            assert!(fs::read_dir(&destination).unwrap().next().is_none());
            assert!(expected.insert(relative(&destination), None).is_none());
        }
    }
    assert_eq!(lab.state(), expected);
    assert!(!lab.home.join("repos/alice/qa/.git").exists());
    assert!(!lab.home.join("store").exists());
    assert!(!lab.home.join("rc/agitd.pid").exists());
}

#[test]
fn rc_land_clone_outcomes_keep_known_categories_and_retain_recovery_evidence() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for (status, code) in [(401, 5), (403, 7), (428, 4), (503, 6), (418, 1)] {
            let lab = Lab::new();
            seed_rc_native_canary(&lab);
            let before = lab.state();
            let git_request = "GET /alice/qa.git/info/refs?service=git-upload-pack";
            let server = Server::start(
                &lab,
                vec![
                    Step::new(
                        "GET /api/agents/alice/qa",
                        Reply::Json(remote(&lab, "alice", AGENT_ID)),
                    ),
                    Step::new(git_request, Reply::Status(status)),
                ],
            );
            let output = rc_land(&lab, mode, "clone-rejected");
            assert_eq!(
                server.finish(),
                vec!["GET /api/agents/alice/qa", git_request]
            );
            let text = assert_rc_failure(&output, mode, code);
            assert!(
                text.contains("could not clone alice/qa for RC settlement"),
                "{text}"
            );
            assert_rc_clone_recovery(&lab, before, true);
            lab.no_requests();
        }
    }
}

#[test]
fn rc_land_invalid_remote_urls_stay_unclassified_and_do_not_reach_git_transport() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        seed_rc_native_canary(&lab);
        let before = lab.state();
        let mut agent = remote(&lab, "alice", AGENT_ID);
        agent["clone_url"] = json!(format!("{}/alice/%2Fqa.git", lab.base));
        let server = Server::start(
            &lab,
            vec![Step::new("GET /api/agents/alice/qa", Reply::Json(agent))],
        );
        let output = rc_land(&lab, mode, "clone-input");
        assert_eq!(server.finish(), vec!["GET /api/agents/alice/qa"]);
        let text = assert_rc_failure(&output, mode, 1);
        assert!(text.contains("encoded path separator"), "{text}");
        assert!(!text.contains("could not clone alice/qa for RC settlement"));
        assert_rc_clone_recovery(&lab, before, false);
        lab.no_requests();
    }
}
