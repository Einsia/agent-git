//! Internal promotion keeps run's validated target ahead of its ordinary stdout.

use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const SOURCE: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const COPY: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const NATIVE: &str = "cccccccc-0000-4000-8000-000000000003";
const PROMOTION_MESSAGES: &[&str] = &[
    "copying alice/qa into your namespace",
    "alice/qa is now yours: me/qa",
    "everything you committed locally survives",
    "keep working: agit commit qa, then agit push qa",
];

struct Hub {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    server: Option<JoinHandle<()>>,
}

impl Hub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::clone(&stop);
        let received = Arc::clone(&requests);
        let server_url = url.clone();
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("local hub accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                let first = request.lines().next().unwrap_or_default();
                let mut fields = first.split_whitespace();
                let route = format!(
                    "{} {}",
                    fields.next().unwrap_or_default(),
                    fields.next().unwrap_or_default()
                );
                received.lock().unwrap().push(route.clone());
                let (status, body) = match route.as_str() {
                    "GET /api/agents/alice/qa" => (
                        "200 OK",
                        json!({
                            "agent_id": SOURCE, "owner": "alice", "name": "qa",
                            "visibility": "public", "session_count": 1,
                            "clone_url": format!("{server_url}/alice/qa.git"),
                        }),
                    ),
                    "POST /api/agents/alice/qa/clone" => (
                        "200 OK",
                        json!({
                            "agent_id": COPY, "forked_from": SOURCE,
                            "owner": "me", "name": "qa",
                            "push_url": format!("{server_url}/me/qa.git"),
                            "web_url": format!("{server_url}/me/qa"),
                        }),
                    ),
                    "GET /api/cli/version" => {
                        ("200 OK", json!({"version": env!("CARGO_PKG_VERSION")}))
                    }
                    _ => (
                        "404 Not Found",
                        json!({"error": "unexpected fixture route", "kind": "not_found"}),
                    ),
                };
                let body = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            stop,
            requests,
            server: Some(server),
        }
    }

    fn assert_promoted(&self) {
        let requests = self.requests.lock().unwrap();
        for expected in [
            "GET /api/agents/alice/qa",
            "POST /api/agents/alice/qa/clone",
        ] {
            assert!(requests.iter().any(|request| request == expected));
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "local hub request ended before its body");
        request.extend_from_slice(&chunk[..read]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]);
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return String::from_utf8(request).unwrap();
        }
    }
}

struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    hub: Hub,
    original_head: String,
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("agit");
        let work = temporary.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let hub = Hub::new();
        let credential = agit::infra::credentials::HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(hub.url.clone()),
            access_token: "synthetic-access".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "synthetic-refresh".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        agit::infra::credentials::save_at(
            &home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(&hub.url).unwrap()
            )),
            &credential,
        )
        .unwrap();
        let source = Repo::init(&home.join("repos/alice/qa")).unwrap();
        source.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let claim = format!("agit-{}", "a".repeat(40));
        let raw = format!(
            "{}\n{}\n",
            json!({"type": "user", "sessionId": NATIVE, "cwd": work,
                "uuid": "fixture-user", "message": {"role": "user", "content": "Preserve this promoted session."}}),
            json!({"type": "assistant", "sessionId": NATIVE, "cwd": work,
                "uuid": "fixture-assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Session preserved."}]}}),
        );
        let events = transcript::wrap_lines(&raw, "claude-code", &claim);
        storage::write_snapshot(source.root(), &events, &events).unwrap();
        let mut snapshot = meta::Meta::new(
            claim,
            "claude-code".into(),
            work.to_string_lossy().into_owned(),
        );
        snapshot.turn = Some(1);
        meta::write(source.root(), &snapshot).unwrap();
        source.add_all().unwrap();
        source.commit("Seed resumable promotion fixture").unwrap();
        source.git(&["branch", "-m", "work"]).unwrap();
        let original_head = source.git(&["rev-parse", "refs/heads/work"]).unwrap();
        identity::pin(&source, &RemoteIdentity::new(&hub.url, SOURCE).unwrap()).unwrap();
        Self {
            temporary,
            home,
            work,
            hub,
            original_head,
        }
    }

    fn output(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temporary.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.url)
            .env(
                "AGIT_SECRETS_KEYSTORE",
                if cfg!(windows) { "os" } else { "file" },
            )
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temporary.path().join("empty-gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work);
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", self.temporary.path());
        }
        command.output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn promoted_repo(&self) -> Repo {
        self.hub.assert_promoted();
        assert!(!self.home.join("repos/alice/qa").exists());
        let promoted = Repo::open(self.home.join("repos/me/qa")).unwrap();
        assert_eq!(
            promoted.git(&["rev-parse", "refs/heads/work"]).unwrap(),
            self.original_head
        );
        promoted
    }
}

#[cfg(windows)]
impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = std::fs::read(self.home.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

#[test]
fn run_mine_echoes_the_promoted_source_before_output_and_keeps_promotion_diagnostics() {
    let lab = Lab::new();
    let output = lab.run(&[
        "run",
        "alice/qa@work",
        "--mine",
        "-b",
        "continued",
        "--no-launch",
    ]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stdout.lines().next(),
        Some("target: me/qa@work (via explicit arguments)")
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with("target:"))
            .count(),
        1
    );
    for message in PROMOTION_MESSAGES {
        assert!(
            stderr.contains(message),
            "missing promotion diagnostic: {stderr}"
        );
        assert!(
            !stdout.contains(message),
            "promotion preceded run output: {stdout}"
        );
    }
    assert!(stdout.contains("forked out me/qa @ continued"), "{stdout}");
    assert!(stdout.contains("materialized VIEW"), "{stdout}");
    let promoted = lab.promoted_repo();
    assert!(promoted.has_ref("refs/heads/continued"));
    let claims =
        link::active_for_branch(&Store::at(lab.home.join("store")), "me", "qa", "continued");
    assert_eq!(
        claims.len(),
        1,
        "the prepared fork must have an active runtime claim"
    );
    assert_eq!(claims[0].source, "claude-code");
}

#[test]
fn standalone_clone_keeps_promotion_progress_on_stdout() {
    let lab = Lab::new();
    let output = lab.run(&["clone", "alice/qa", "--mine", "--no-bind"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stdout.starts_with("copying alice/qa into your namespace"),
        "{stdout}"
    );
    assert!(!stdout.lines().any(|line| line.starts_with("target:")));
    for message in PROMOTION_MESSAGES {
        assert!(
            stdout.contains(message),
            "missing standalone clone output: {stdout}"
        );
        assert!(
            !stderr.contains(message),
            "standalone clone output moved: {stderr}"
        );
    }
    assert!(stdout.contains(&format!("{}/me/qa", lab.hub.url)));
    assert!(!lab.promoted_repo().has_ref("refs/heads/continued"));
}

fn legacy_streams(output: &Output, mode: &str) -> (String, String) {
    if mode != "--json" {
        return (
            String::from_utf8(output.stdout.clone()).unwrap(),
            String::from_utf8(output.stderr.clone()).unwrap(),
        );
    }
    assert!(
        output.stderr.is_empty(),
        "JSON diagnostics escaped the envelope"
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], "cli-output");
    assert_eq!(document["command"], "open");
    assert_eq!(document["ok"], output.status.success());
    assert_eq!(document["exit_code"], output.status.code().unwrap());
    assert_eq!(document["result"]["format"], "text");
    let stdout = document["result"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let stderr = document["diagnostics"]["stderr"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["message"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    (stdout, stderr)
}

fn assert_legacy_acquisition(stdout: &str, stderr: &str, selected: bool) {
    assert!(
        stdout.starts_with("copying alice/qa into your namespace"),
        "{stdout}"
    );
    assert!(!stdout.lines().any(|line| line.starts_with("target:")));
    assert!(!stderr.lines().any(|line| line.starts_with("target:")));
    for message in PROMOTION_MESSAGES {
        assert!(
            stdout.contains(message),
            "acquisition output changed: {stdout}"
        );
        assert!(
            !stderr.contains(message),
            "acquisition output moved to diagnostics: {stderr}"
        );
    }
    let acquired = stdout.find("keep working: agit commit qa").unwrap();
    let fetched = stdout.find("fetched the latest history of me/qa").unwrap();
    assert!(acquired < fetched, "{stdout}");
    if selected {
        assert!(fetched < stdout.find("arbitration:").unwrap(), "{stdout}");
    } else {
        assert!(!stdout.contains("arbitration:"), "{stdout}");
    }
}

#[test]
fn legacy_run_mine_preserves_acquisition_stdout_without_a_target_echo() {
    for mode in ["--quiet", "--json"]
        .into_iter()
        .filter(|mode| *mode == "--quiet" || cfg!(any(unix, all(windows, target_env = "msvc"))))
    {
        let lab = Lab::new();
        let output = lab.run(&[
            mode,
            "run",
            "alice/qa@work",
            "--mine",
            "-b",
            "continued",
            "--no-launch",
        ]);
        let (stdout, stderr) = legacy_streams(&output, mode);
        assert_legacy_acquisition(&stdout, &stderr, true);
        assert_eq!(
            stdout.contains("forked out me/qa @ continued"),
            mode == "--json",
            "{stdout}"
        );
        assert!(stdout.contains("materialized VIEW"), "{stdout}");
        assert!(lab.promoted_repo().has_ref("refs/heads/continued"));
    }
}

#[test]
fn legacy_invalid_run_mine_keeps_acquisition_but_refuses_before_the_fork_prompt() {
    for mode in ["--quiet", "--json"]
        .into_iter()
        .filter(|mode| *mode == "--quiet" || cfg!(any(unix, all(windows, target_env = "msvc"))))
    {
        for name_fork in [false, true] {
            let lab = Lab::new();
            let mut args = vec![mode, "run", "alice/qa@absent", "--mine", "--no-launch"];
            if name_fork {
                args.extend(["-b", "continued"]);
            }
            let output = lab.output(&args);
            assert_eq!(
                output.status.code(),
                Some(agit::ExitCode::Ref.as_i32()),
                "{output:?}"
            );
            let (stdout, stderr) = legacy_streams(&output, mode);
            assert_legacy_acquisition(&stdout, &stderr, false);
            assert!(!stdout.contains("forked out"), "{stdout}");
            assert!(
                stderr.contains("failed to resolve `me/qa@absent`"),
                "{stderr}"
            );
            assert!(
                !stderr.contains("non-interactive runs must pass -b"),
                "{stderr}"
            );
            assert!(!lab.promoted_repo().has_ref("refs/heads/continued"));
        }
    }
}
